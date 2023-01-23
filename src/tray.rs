use log::trace;
use tray_item::TrayItem;

pub struct Tray {
    pub tray_item: TrayItem,
}

impl Tray {
    pub fn new() -> Result<Self, String> {
        trace!("Creating tray item");
        let app_name = "VCM";
        let icon_path = "";
        // Listen for appearance changes and remake tray https://github.com/frewsxcv/rust-dark-light/pull/26/files
        // let icon_path = match config.appearance {
        //     dark_light::Mode::Dark => {}
        //     dark_light::Mode::Light => {}
        // dark_light::Mode::Default => {}
        // };
        match TrayItem::new(app_name, icon_path) {
            Ok(tray_item) => {
                trace!("Tray item created");
                let tray = Self { tray_item };
                trace!("Tray item configured");
                Ok(tray)
            }
            Err(_) => Err("Failed to create system tray item {:?}".to_string()),
        }
    }

    pub fn add_mic_mute<F>(&mut self, callback: F) -> &mut Self
    where
        F: Fn() + Send + 'static,
    {
        self.tray_item
            .add_menu_item("Mute", callback)
            .expect("Failed to create mute tray item");
        trace!("Tray menu item created to mute");
        self
    }

    pub fn add_mic_unmute<F>(&mut self, callback: F) -> &mut Self
    where
        F: Fn() + Send + 'static,
    {
        self.tray_item
            .add_menu_item("Unmute", callback)
            .expect("Failed to create unmute tray item");
        trace!("Tray menu item created to unmute");
        self
    }

    pub fn run(&mut self) {
        let inner = self.tray_item.inner_mut();
        inner.add_quit_item("Quit");
        trace!("Starting tray");
        inner.display();
    }
}
