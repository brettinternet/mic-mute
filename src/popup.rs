use anyhow::{Context, Result};
// use cocoa::{
//     appkit::{NSApp, NSApplication, NSApplicationActivationPolicy::*, NSScreen},
//     base::nil,
// };
use crate::utils::get_cursor_pos;
use log::trace;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event_loop::EventLoop,
    monitor::MonitorHandle,
    platform::macos::{EventLoopWindowTargetExtMacOS, WindowBuilderExtMacOS},
    window::{Window, WindowBuilder},
};

const MUTE_TEXT: &str = "Muted";
const UNMUTE_TEXT: &str = "Unmuted";

pub fn get_mute_title_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

pub struct Popup {
    window: Window,
}

impl Popup {
    pub fn new(event_loop: &EventLoop<()>, muted: bool) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(get_mute_title_text(muted))
            .with_titlebar_hidden(true)
            .with_movable_by_window_background(true)
            .with_always_on_top(true)
            .with_closable(false)
            .with_content_protection(true)
            .with_decorations(false)
            .with_maximized(false)
            .with_minimizable(false)
            .with_resizable(false)
            .with_position(LogicalPosition::new(50, 50))
            .with_inner_size(LogicalSize::new(200, 40))
            .with_visible_on_all_workspaces(true)
            .with_visible(false)
            .build(event_loop)
            .context("Failed to build window")?;
        window.set_ignore_cursor_events(true)?;
        let mut popup = Self { window };
        popup.update_placement()?;
        Ok(popup)
    }

    pub fn update(&mut self, muted: bool) -> Result<&mut Self> {
        self.window.set_title(get_mute_title_text(muted));
        self.update_placement()?;
        if muted {
            self.window.set_visible(true);
        }
        Ok(self)
    }

    pub fn hide(&mut self) -> Result<&mut Self> {
        self.window.set_visible(false);
        Ok(self)
    }

    pub fn update_placement(&mut self) -> Result<&mut Self> {
        let size = PhysicalSize::new(200, 40);
        self.window.set_inner_size(size);
        let monitor = self.get_current_monitor()?;
        self.window
            .set_outer_position(Popup::get_position(monitor, size));
        Ok(self)
    }

    fn get_current_monitor(&self) -> Result<Option<MonitorHandle>> {
        if let Some((x, y)) = get_cursor_pos() {
            trace!("Found cursor position {:?}", (x, y));
            let monitor = self.window.monitor_from_point(x.into(), y.into());
            Ok(monitor)
        } else {
            Ok(None)
        }
    }

    fn get_position(
        monitor: Option<MonitorHandle>,
        window_size: PhysicalSize<i32>,
    ) -> PhysicalPosition<f32> {
        if let Some(monitor) = monitor {
            let monitor_position = monitor.position();
            let monitor_size = monitor.size();
            let [size_width, size_height] =
                [monitor_size.width, monitor_size.height].map(|n| n as f32);
            let [position_x, position_y, window_size_width, window_size_height] = [
                monitor_position.x,
                monitor_position.y,
                window_size.width,
                window_size.height,
            ]
            .map(|n| n as f32);
            let x: f32 = ((size_width + position_x) / 2.) - (window_size_width / 2.);
            let y: f32 = (position_y + size_height) - (window_size_height * 2.);
            trace!("Setting window position {:?}", (x, y));
            PhysicalPosition::new(x, y)
        } else {
            PhysicalPosition::new(0., 0.)
        }
    }
}
