use crate::config::AppVars;
use crate::icons::{self, IconVariant, TrayImage};
use crate::mic::{DeviceState, DeviceStatus};
use crate::settings::ExcludedDevice;
use crate::shortcuts::Shortcut;
use anyhow::{Context, Result};
use log::trace;
use muda::accelerator::{Key, KeyAccelerator};
use muda::{
    accelerator::Accelerator, CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use std::fmt;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";
/// The tooltip's last line while a camera is in use, which explains the icon's badge.
const CAMERA_IN_USE_TEXT: &str = "Camera in use";
/// The submenu with a row per input device.
const DEVICES_MENU_TEXT: &str = "Microphones";
/// Its disabled only row while no device is connected or excluded.
const NO_DEVICES_TEXT: &str = "No input devices";
const OPEN_SOUND_SETTINGS_TEXT: &str = "Open Sound Settings…";
/// The state of an excluded device that isn't connected.
const NOT_CONNECTED_TEXT: &str = "Excluded, not connected";

/// The words after a device's name in the Microphones menu.
fn status_text(status: DeviceStatus) -> &'static str {
    match status {
        DeviceStatus::Muted => "Muted",
        DeviceStatus::MutedByVolume => "Muted by input volume",
        DeviceStatus::Unmuted => "Unmuted",
        DeviceStatus::IgnoresWrites => "Skipped: ignores mute",
        DeviceStatus::NoControl => "Skipped: no mute control",
        DeviceStatus::Failed => "Failed",
        DeviceStatus::Excluded => "Excluded",
    }
}

/// `text` for a muda item, which drops a single `&` as a Windows mnemonic marker.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// One row of the Microphones menu: a check item, checked while Mic Mute controls the device.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceRow {
    /// The UID a click excludes or includes; `None` for a device whose UID can't be read.
    /// Such a device can't be excluded, so its row is disabled.
    uid: Option<String>,
    /// The name saved with an exclusion, so the row can show it while the device is away.
    name: String,
    text: String,
    /// Checked: the device isn't excluded.
    is_controlled: bool,
}

/// The Microphones menu's rows: the connected input devices in `states`, then the excluded
/// devices that aren't connected, so they can be included again. A connected device that
/// shares its name with another shows its UID too.
fn device_rows(states: &[DeviceState], excluded: &[ExcludedDevice]) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = states
        .iter()
        .map(|state| {
            let shares_name = states
                .iter()
                .filter(|other| other.name == state.name)
                .count()
                > 1;
            let label = match &state.uid {
                Some(uid) if shares_name => format!("{} ({})", state.name, uid),
                _ => state.name.clone(),
            };
            DeviceRow {
                uid: state.uid.clone(),
                name: state.name.clone(),
                text: format!("{} — {}", label, status_text(state.status)),
                is_controlled: state.status != DeviceStatus::Excluded,
            }
        })
        .collect();
    for device in excluded {
        let is_listed = rows
            .iter()
            .any(|row| row.uid.as_deref() == Some(device.uid.as_str()));
        if device.uid.is_empty() || is_listed {
            continue;
        }
        let label = if device.name.is_empty() {
            &device.uid
        } else {
            &device.name
        };
        rows.push(DeviceRow {
            uid: Some(device.uid.clone()),
            name: device.name.clone(),
            text: format!("{label} — {NOT_CONNECTED_TEXT}"),
            is_controlled: false,
        });
    }
    rows
}

/// Whether `rows` fit the items that show `shown`: the same devices in the same order, so
/// only texts and check marks may need an update.
fn has_same_devices(shown: &[DeviceRow], rows: &[DeviceRow]) -> bool {
    shown.len() == rows.len()
        && shown
            .iter()
            .zip(rows)
            .all(|(shown, row)| shown.uid == row.uid)
}

/// The Microphones submenu: a check item per device row, or a disabled placeholder while there
/// are none, then a separator and Open Sound Settings, which stay put.
struct DevicesMenu {
    submenu: Submenu,
    /// The device rows' items, first in the submenu, in order.
    items: Vec<CheckMenuItem>,
    /// In the submenu in place of `items` while there are no rows.
    placeholder: Option<MenuItem>,
    /// What `items` show; `update` leaves matching items alone. `None` while a failed rebuild
    /// left the submenu half built, so the next update rebuilds it.
    shown: Option<Vec<DeviceRow>>,
    open_sound_settings: MenuItem,
}

impl DevicesMenu {
    fn new() -> Result<Self> {
        let submenu = Submenu::new(DEVICES_MENU_TEXT, true);
        let placeholder = MenuItem::new(NO_DEVICES_TEXT, false, None);
        let open_sound_settings = MenuItem::new(OPEN_SOUND_SETTINGS_TEXT, true, None);
        submenu
            .append_items(&[
                &placeholder,
                &PredefinedMenuItem::separator(),
                &open_sound_settings,
            ])
            .context("Failed to build the Microphones menu")?;
        Ok(Self {
            submenu,
            items: Vec::new(),
            placeholder: Some(placeholder),
            shown: Some(Vec::new()),
            open_sound_settings,
        })
    }

    /// Shows `rows`, rebuilding the device rows only when the devices change. A click flips its
    /// item's check mark before the event loop sees it, so check marks are compared with the
    /// items themselves.
    fn update(&mut self, rows: &[DeviceRow]) -> Result<()> {
        let Some(shown) = self
            .shown
            .as_mut()
            .filter(|shown| has_same_devices(shown, rows))
        else {
            return self.rebuild(rows);
        };
        for ((item, shown), row) in self.items.iter().zip(shown).zip(rows) {
            if shown.text != row.text {
                item.set_text(menu_text(&row.text));
            }
            if item.is_checked() != row.is_controlled {
                item.set_checked(row.is_controlled);
            }
            shown.clone_from(row);
        }
        Ok(())
    }

    /// Replaces the device rows, or the placeholder, with items for `rows`. Every item that
    /// stays in the submenu stays in `items` or `placeholder`, so a failure can't orphan one.
    fn rebuild(&mut self, rows: &[DeviceRow]) -> Result<()> {
        self.shown = None;
        while let Some(item) = self.items.pop() {
            if let Err(err) = self.submenu.remove(&item) {
                self.items.push(item);
                return Err(err).context("Failed to remove a Microphones menu row");
            }
        }
        if let Some(placeholder) = self.placeholder.take() {
            if let Err(err) = self.submenu.remove(&placeholder) {
                self.placeholder = Some(placeholder);
                return Err(err).context("Failed to remove the Microphones menu placeholder");
            }
        }
        if rows.is_empty() {
            let placeholder = MenuItem::new(NO_DEVICES_TEXT, false, None);
            self.submenu
                .prepend(&placeholder)
                .context("Failed to add the Microphones menu placeholder")?;
            self.placeholder = Some(placeholder);
        }
        for (position, row) in rows.iter().enumerate() {
            let item = CheckMenuItem::new(
                menu_text(&row.text),
                row.uid.is_some(),
                row.is_controlled,
                None,
            );
            self.submenu
                .insert(&item, position)
                .context("Failed to add a Microphones menu row")?;
            self.items.push(item);
        }
        self.shown = Some(rows.to_vec());
        trace!("Rebuilt the Microphones menu with {} row(s)", rows.len());
        Ok(())
    }

    /// The device of the row whose item has `id`; `None` for any other item.
    fn device_for(&self, id: &MenuId) -> Option<ExcludedDevice> {
        let index = self.items.iter().position(|item| item.id() == id)?;
        let row = self.shown.as_ref()?.get(index)?;
        Some(ExcludedDevice {
            uid: row.uid.clone()?,
            name: row.name.clone(),
        })
    }
}

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
    /// The Microphones submenu, below the Mute item.
    devices: DevicesMenu,
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
        // Lists no device until the first poll's `update_devices`.
        let devices = DevicesMenu::new()?;

        tray_menu
            .append_items(&[
                &toggle_mute,
                &devices.submenu,
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
            devices,
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

    pub fn open_sound_settings_id(&self) -> &MenuId {
        self.devices.open_sound_settings.id()
    }

    /// Shows `states`, the connected input devices, and then the `excluded` devices that aren't
    /// connected, in the Microphones menu. Touches only the rows that change, so a repeated
    /// call with the same rows costs nothing.
    pub fn update_devices(
        &mut self,
        states: &[DeviceState],
        excluded: &[ExcludedDevice],
    ) -> Result<()> {
        self.devices.update(&device_rows(states, excluded))
    }

    /// The device a click on the Microphones menu item `id` excludes or includes, with the name
    /// to save; `None` when `id` isn't one of its device rows.
    pub fn clicked_device(&self, id: &MenuId) -> Option<ExcludedDevice> {
        self.devices.device_for(id)
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

    fn state(name: &str, status: DeviceStatus) -> DeviceState {
        DeviceState {
            uid: Some(format!("{name} UID")),
            name: name.to_string(),
            status,
        }
    }

    fn excluded(uid: &str, name: &str) -> ExcludedDevice {
        ExcludedDevice {
            uid: uid.to_string(),
            name: name.to_string(),
        }
    }

    fn texts(rows: &[DeviceRow]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn device_rows_name_each_device_and_its_state() {
        use DeviceStatus::*;
        let states = [
            state("MacBook Pro Microphone", Muted),
            state("USB Mic", MutedByVolume),
            state("Steam Streaming Microphone", Unmuted),
            state("Microsoft Teams Audio", IgnoresWrites),
            state("iPhone Microphone", NoControl),
            state("Broken Mic", Failed),
            state("Steam Streaming Speakers", Excluded),
        ];

        let rows = device_rows(&states, &[]);

        assert_eq!(
            texts(&rows),
            [
                "MacBook Pro Microphone — Muted",
                "USB Mic — Muted by input volume",
                "Steam Streaming Microphone — Unmuted",
                "Microsoft Teams Audio — Skipped: ignores mute",
                "iPhone Microphone — Skipped: no mute control",
                "Broken Mic — Failed",
                "Steam Streaming Speakers — Excluded",
            ]
        );
        let controlled: Vec<bool> = rows.iter().map(|row| row.is_controlled).collect();
        assert_eq!(controlled, [true, true, true, true, true, true, false]);
        assert_eq!(rows[0].uid.as_deref(), Some("MacBook Pro Microphone UID"));
    }

    #[test]
    fn device_rows_list_excluded_devices_that_are_not_connected_once() {
        let states = [
            state("Built-in", DeviceStatus::Muted),
            state("Teams", DeviceStatus::Excluded),
        ];
        let excluded = [
            excluded("Teams UID", "Teams"),
            excluded("Headset UID", "Headset"),
            excluded("Bare UID", ""),
            excluded("Headset UID", "Headset"),
            excluded("", "No UID"),
        ];

        let rows = device_rows(&states, &excluded);

        assert_eq!(
            texts(&rows),
            [
                "Built-in — Muted",
                "Teams — Excluded",
                "Headset — Excluded, not connected",
                "Bare UID — Excluded, not connected",
            ]
        );
        assert_eq!(rows[2].uid.as_deref(), Some("Headset UID"));
        assert_eq!(rows[2].name, "Headset");
        assert!(!rows[2].is_controlled);
        assert!(!rows[3].is_controlled);
    }

    #[test]
    fn device_rows_tell_devices_with_the_same_name_apart_by_uid() {
        let usb = |uid: &str| DeviceState {
            uid: Some(uid.to_string()),
            name: "USB Microphone".to_string(),
            status: DeviceStatus::Muted,
        };
        let states = [
            usb("USB-1"),
            usb("USB-2"),
            state("Built-in", DeviceStatus::Muted),
        ];

        assert_eq!(
            texts(&device_rows(&states, &[])),
            [
                "USB Microphone (USB-1) — Muted",
                "USB Microphone (USB-2) — Muted",
                "Built-in — Muted",
            ]
        );
    }

    #[test]
    fn device_row_without_a_uid_stays_controlled_and_cannot_be_excluded() {
        let states = [DeviceState {
            uid: None,
            name: "No UID".to_string(),
            status: DeviceStatus::Unmuted,
        }];

        let rows = device_rows(&states, &[]);

        assert_eq!(rows[0].uid, None);
        assert!(rows[0].is_controlled);
    }

    #[test]
    fn menu_text_keeps_ampersands_in_device_names() {
        assert_eq!(
            menu_text("Rock & Roll Mic — Muted"),
            "Rock && Roll Mic — Muted"
        );
    }

    #[test]
    fn rows_for_the_same_devices_need_no_rebuild() {
        let rows = |states: &[DeviceState]| device_rows(states, &[]);
        let muted = rows(&[
            state("Built-in", DeviceStatus::Muted),
            state("USB", DeviceStatus::Muted),
        ]);
        let changed = rows(&[
            state("Built-in", DeviceStatus::Unmuted),
            state("USB", DeviceStatus::Excluded),
        ]);
        let unplugged = rows(&[state("Built-in", DeviceStatus::Muted)]);
        let reordered = rows(&[
            state("USB", DeviceStatus::Muted),
            state("Built-in", DeviceStatus::Muted),
        ]);

        assert!(has_same_devices(&muted, &changed));
        assert!(!has_same_devices(&muted, &unplugged));
        assert!(!has_same_devices(&muted, &reordered));
        assert!(has_same_devices(&[], &[]));
    }
}
