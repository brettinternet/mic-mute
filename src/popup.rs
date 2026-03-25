use crate::event_loop::EventLoopMessage;
use crate::popup_content::PopupContent;
use crate::utils::get_cursor_pos;
use anyhow::{Context, Result};
use cocoa::{
    appkit::{NSView, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility},
    base::{id, YES},
};
use log::trace;
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    monitor::MonitorHandle,
    platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS},
    window::{Theme, Window, WindowBuilder},
};

const MUTED_TITLE: &str = "Muted";
const UNMUTED_TITLE: &str = "Unmuted";

pub type WindowSize<T = f64> = PhysicalSize<T>;

fn get_mute_title_text(muted: bool) -> &'static str {
    if muted {
        MUTED_TITLE
    } else {
        UNMUTED_TITLE
    }
}

fn setup_window(window: id) {
    unsafe {
        window.setHasShadow_(true);
        // Rounded edges hack: https://stackoverflow.com/a/37418915
        let mask = window.styleMask();
        let _: () = msg_send![
            window,
            setStyleMask: mask
                | NSWindowStyleMask::NSTitledWindowMask
                | NSWindowStyleMask::NSFullSizeContentViewWindowMask
        ];
        let _: () = msg_send![
            window,
            setTitleVisibility: NSWindowTitleVisibility::NSWindowTitleHidden
        ];
        let _: () = msg_send![window, setTitlebarAppearsTransparent: YES];
    };
}

pub struct Popup {
    window: Window,
    content: PopupContent,
    pub cursor_on_separate_monitor: bool,
}

impl Popup {
    pub fn new(event_loop: &EventLoopMessage, mic_muted: bool) -> Result<Self> {
        let camera_muted = false;
        let window = WindowBuilder::new()
            .with_title(get_mute_title_text(mic_muted))
            .with_titlebar_hidden(true)
            .with_movable_by_window_background(true)
            .with_always_on_top(true)
            .with_closable(false)
            .with_content_protection(true)
            .with_decorations(false)
            .with_maximized(false)
            .with_minimizable(false)
            .with_resizable(false)
            .with_visible_on_all_workspaces(true)
            .with_visible(false)
            .with_has_shadow(true)
            .build(event_loop)
            .context("Failed to build window")?;
        window.set_ignore_cursor_events(true)?;
        let size = Popup::get_size(window.scale_factor());
        window.set_inner_size(size);

        let scale = window.scale_factor();
        trace!("Window scale factor {}", scale);
        let content = PopupContent::new(
            mic_muted,
            camera_muted,
            size.to_logical(scale),
            window.theme(),
        )?;
        unsafe {
            let ns_view = window.ns_view() as id;
            ns_view.addSubview_(content.view);
            let _: () = msg_send![content.view, release];
            let ns_window = window.ns_window() as id;
            setup_window(ns_window);
        };

        let popup = Self {
            window,
            content,
            cursor_on_separate_monitor: false,
        };
        Ok(popup)
    }

    fn get_size(scale: f64) -> WindowSize {
        LogicalSize::new(250., 40.).to_physical(scale)
    }

    pub fn get_theme(&self) -> Theme {
        self.window.theme()
    }

    pub fn update_with_camera(
        &mut self,
        mic_muted: bool,
        camera_muted: bool,
        active_device_name: Option<&str>,
    ) -> Result<&mut Self> {
        self.window.set_title(get_mute_title_text(mic_muted));
        self.update_placement()?;
        self.content.update(
            mic_muted,
            camera_muted,
            self.get_theme(),
            active_device_name,
        )?;
        if mic_muted {
            self.window.set_visible(true);
            unsafe {
                let ns_window = self.window.ns_window() as id;
                let _: () = msg_send![ns_window, orderFrontRegardless];
            }
        }
        Ok(self)
    }

    pub fn hide(&mut self) -> Result<&mut Self> {
        self.window.set_visible(false);
        Ok(self)
    }

    pub fn update_placement(&mut self) -> Result<&mut Self> {
        if let Some(monitor) = self.get_current_monitor()? {
            let size = Popup::get_size(monitor.scale_factor());
            self.window.set_inner_size(size);
            self.cursor_on_separate_monitor = false;
            self.window
                .set_outer_position(self.get_position(monitor, size));
        }
        Ok(self)
    }

    pub fn detect_cursor_monitor(&mut self) -> Result<&mut Self> {
        if let Some(cursor_monitor) = self.get_current_monitor()? {
            if let Some(window_monitor) = self.window.current_monitor() {
                self.cursor_on_separate_monitor = window_monitor.name() != cursor_monitor.name()
            }
        }
        Ok(self)
    }

    fn get_current_monitor(&self) -> Result<Option<MonitorHandle>> {
        if let Some((x, y)) = get_cursor_pos() {
            let monitor = self.window.monitor_from_point(x.into(), y.into());
            Ok(monitor)
        } else {
            Ok(None)
        }
    }

    fn get_position(
        &self,
        monitor: MonitorHandle,
        window_size: WindowSize,
    ) -> PhysicalPosition<f64> {
        let monitor_position: PhysicalPosition<f64> = monitor.position().cast();
        let monitor_size: PhysicalSize<f64> = monitor.size().cast();
        let x: f64 = (monitor_position.x + (monitor_size.width / 2.)) - (window_size.width / 2.);
        let y: f64 = (monitor_position.y + monitor_size.height) - (window_size.height * 2.);
        PhysicalPosition::new(x, y)
    }
}
