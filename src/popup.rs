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
    window::{Window, WindowBuilder},
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
                | NSWindowStyleMask::NSFullSizeContentViewWindowMask // NSWindowStyleMask::NSResizableWindowMask
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
    pub fn new(event_loop: &EventLoopMessage, muted: bool) -> Result<Self> {
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
            .with_visible_on_all_workspaces(true)
            .with_visible(false)
            // Doesn't work
            .with_has_shadow(true)
            .build(event_loop)
            .context("Failed to build window")?;
        window.set_ignore_cursor_events(true)?;
        let size = Popup::get_size(window.scale_factor());
        window.set_inner_size(size);

        let scale = window.scale_factor();
        trace!("Window scale factor {}", scale);
        let content = PopupContent::new(muted, size.to_logical(scale))?;
        unsafe {
            let ns_view = window.ns_view() as id;
            ns_view.addSubview_(content.view);
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
        // PhysicalSize::new(400., 80.)
        LogicalSize::new(200., 40.).to_physical(scale)
    }

    /// TODO: add blur?
    /// https://github.com/rust-windowing/winit/issues/538
    /// https://github.com/servo/core-foundation-rs/blob/master/cocoa/examples/nsvisualeffectview_blur.rs

    pub fn update(&mut self, muted: bool) -> Result<&mut Self> {
        self.window.set_title(get_mute_title_text(muted));
        self.update_placement()?;
        self.content.update(muted)?;
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
