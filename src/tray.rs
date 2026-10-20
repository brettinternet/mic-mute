use crate::config::AppVars;
use crate::icons::{rasterize_svg, tray_icon_color};
use crate::mic::InputDevice;
use crate::settings::{Settings, ShortcutConfig};
use anyhow::{Context, Result};
use log::trace;
use muda::{
    accelerator::Accelerator, CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::fmt;
use tao::window::Theme;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";
const NO_MICS_TEXT: &str = "No Input Devices";

/// One row of the Excluded Mics submenu.
#[derive(Clone, PartialEq)]
struct ExcludedMicEntry {
    device: InputDevice,
    connected: bool,
}

/// Connected devices first, then excluded devices that aren't connected so
/// they can still be unticked.
fn excluded_mic_entries(devices: &[InputDevice], settings: &Settings) -> Vec<ExcludedMicEntry> {
    let mut entries: Vec<ExcludedMicEntry> = devices
        .iter()
        .map(|device| ExcludedMicEntry {
            device: device.clone(),
            connected: true,
        })
        .collect();
    for uid in &settings.excluded_devices {
        if entries.iter().any(|e| e.device.uid == *uid) {
            continue;
        }
        entries.push(ExcludedMicEntry {
            device: InputDevice {
                uid: uid.clone(),
                name: uid.clone(),
            },
            connected: false,
        });
    }
    entries
}

pub fn get_mute_menu_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

fn get_image(muted: bool, _theme: Theme) -> Result<(Vec<u8>, u32, u32)> {
    const MIC_ON: &[u8] = include_bytes!("../assets/mic.svg");
    const MIC_OFF: &[u8] = include_bytes!("../assets/mic-off.svg");
    let svg = if muted { MIC_OFF } else { MIC_ON };
    rasterize_svg(svg, &tray_icon_color(muted))
}

fn get_icon(muted: bool, theme: Theme) -> Result<Icon> {
    trace!("Fetching icons");
    let (icon_rgba, icon_width, icon_height) = get_image(muted, theme)?;
    let icon =
        Icon::from_rgba(icon_rgba, icon_width, icon_height).context("Failed to open icon")?;
    Ok(icon)
}

fn accelerator_from_config(config: &ShortcutConfig) -> Accelerator {
    let parts = config
        .modifiers
        .iter()
        .map(|modifier| match modifier.as_str() {
            "meta" => "cmd".to_string(),
            other => other.to_string(),
        });
    let accelerator = parts
        .chain(std::iter::once(config.key.clone()))
        .collect::<Vec<_>>()
        .join("+");

    accelerator
        .parse::<Accelerator>()
        .unwrap_or_else(|_| "A".parse().unwrap())
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
    pub launch_at_login: CheckMenuItem,
    pub show_in_dock: CheckMenuItem,
    pub show_popup: CheckMenuItem,
    excluded_mics: Submenu,
    excluded_mic_entries: Vec<ExcludedMicEntry>,
    excluded_mic_items: Vec<(CheckMenuItem, InputDevice)>,
    pub about: MenuItem,
    pub quit: MenuItem,
}

impl Tray {
    pub fn new(muted: bool, theme: Theme, app_vars: AppVars, settings: &Settings) -> Result<Self> {
        trace!("Creating tray icon");
        let icon = get_icon(muted, theme)?;
        let tray_menu = Menu::new();
        let toggle_mute = MenuItem::new(
            get_mute_menu_text(muted),
            true,
            Some(accelerator_from_config(&settings.mic_shortcut)),
        );
        let launch_at_login =
            CheckMenuItem::new("Launch at Login", true, settings.launch_at_login, None);
        let show_in_dock = CheckMenuItem::new("Show in Dock", true, settings.show_in_dock, None);
        let show_popup = CheckMenuItem::new("Show Popup", true, settings.show_popup, None);
        let excluded_mics = Submenu::new("Excluded Mics", true);
        excluded_mics
            .append(&MenuItem::new(NO_MICS_TEXT, false, None))
            .context("Failed to append excluded mics placeholder")?;
        let about = MenuItem::new("About", true, None);
        let quit = MenuItem::new("Exit", true, None);

        tray_menu
            .append_items(&[
                &toggle_mute,
                &PredefinedMenuItem::separator(),
                &launch_at_login,
                &show_in_dock,
                &show_popup,
                &excluded_mics,
                &about,
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
            launch_at_login,
            show_in_dock,
            show_popup,
            excluded_mics,
            excluded_mic_entries: Vec::new(),
            excluded_mic_items: Vec::new(),
            about,
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

    /// Rebuild the Excluded Mics submenu when devices change and sync its checkboxes.
    pub fn update_excluded_mics(
        &mut self,
        devices: &[InputDevice],
        settings: &Settings,
    ) -> Result<()> {
        let entries = excluded_mic_entries(devices, settings);
        if entries != self.excluded_mic_entries {
            while self.excluded_mics.remove_at(0).is_some() {}
            self.excluded_mic_items.clear();
            if entries.is_empty() {
                self.excluded_mics
                    .append(&MenuItem::new(NO_MICS_TEXT, false, None))
                    .context("Failed to append excluded mics placeholder")?;
            }
            for entry in &entries {
                let text = if !entry.connected {
                    format!("{} (Not Connected)", entry.device.uid)
                } else if entries
                    .iter()
                    .filter(|e| e.device.name == entry.device.name)
                    .count()
                    > 1
                {
                    format!("{} ({})", entry.device.name, entry.device.uid)
                } else {
                    entry.device.name.clone()
                };
                let item = CheckMenuItem::new(text, true, false, None);
                self.excluded_mics
                    .append(&item)
                    .context("Failed to append excluded mic item")?;
                self.excluded_mic_items.push((item, entry.device.clone()));
            }
            self.excluded_mic_entries = entries;
            trace!("Rebuilt excluded mics menu");
        }
        for (item, device) in &self.excluded_mic_items {
            item.set_checked(settings.is_device_excluded(&device.uid));
        }
        Ok(())
    }

    /// The device for an Excluded Mics menu item, if `id` is one.
    pub fn excluded_mic_for(&self, id: &MenuId) -> Option<InputDevice> {
        self.excluded_mic_items
            .iter()
            .find(|(item, _)| item.id() == id)
            .map(|(_, device)| device.clone())
    }

    /// Update the displayed keyboard shortcuts after settings change.
    pub fn update_accelerators(&mut self, mic_shortcut: &ShortcutConfig) -> Result<()> {
        self.toggle_mute
            .set_accelerator(Some(accelerator_from_config(mic_shortcut)))
            .context("Failed to update mic accelerator")?;
        Ok(())
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

    #[test]
    fn test_excluded_mic_entries_keeps_disconnected_exclusions() {
        let settings = Settings {
            excluded_devices: vec!["uid-2".to_string(), "uid-3".to_string()],
            ..Settings::default()
        };
        let devices = vec![
            InputDevice {
                uid: "uid-1".into(),
                name: "USB Microphone".into(),
            },
            InputDevice {
                uid: "uid-2".into(),
                name: "USB Microphone".into(),
            },
        ];

        let entries = excluded_mic_entries(&devices, &settings);

        let rows: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.device.uid.as_str(),
                    e.connected,
                    settings.is_device_excluded(&e.device.uid),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("uid-1", true, false),
                ("uid-2", true, true),
                ("uid-3", false, true),
            ]
        );
    }
}
