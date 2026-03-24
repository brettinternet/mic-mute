use crate::event_loop::EventLoopMessage;
use crate::popup_content::PopupContent;
use crate::utils::get_cursor_pos;
use anyhow::{Context, Result};
use cocoa::{
    appkit::{NSView, NSWindow},
    base::{id, NO, YES},
    foundation::{NSPoint, NSRect, NSSize},
};
use log::trace;
use tao::{
    dpi::{LogicalSize, PhysicalSize},
    monitor::MonitorHandle,
    platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS},
    window::{Theme, Window, WindowBuilder},
};

const MUTED_TITLE: &str = "Muted";
const UNMUTED_TITLE: &str = "Unmuted";

pub type WindowSize<T = f64> = PhysicalSize<T>;

fn get_mute_title_text(muted: bool) -> &'static str {
    if muted { MUTED_TITLE } else { UNMUTED_TITLE }
}

fn setup_window(window: id, view: id) {
    unsafe {
        window.setHasShadow_(true);
        // Transparent window so NSVisualEffectView blends with the desktop behind it,
        // and the CALayer rounded corners show the desktop through the corner regions.
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window, setBackgroundColor: clear];
        let _: () = msg_send![window, setOpaque: NO];
        // Rounded corners on the content view via CALayer.
        // No NSTitledWindowMask is used, so the window height stays exactly as set
        // by set_inner_size without any titlebar height being added.
        let _: () = msg_send![view, setWantsLayer: YES];
        let layer: id = msg_send![view, layer];
        let _: () = msg_send![layer, setCornerRadius: 10.0_f64];
        let _: () = msg_send![layer, setMasksToBounds: YES];
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
        let logical: LogicalSize<f64> = size.to_logical(scale);
        let content = PopupContent::new(mic_muted, camera_muted, logical, window.theme())?;

        unsafe {
            let ns_view = window.ns_view() as id;
            let ns_window = window.ns_window() as id;

            // Set the window to the exact logical size FIRST, before adding subviews
            // with autoresizing masks. This ensures subviews start at the correct size
            // so autoresizing math stays correct if the window is later resized.
            let _: () = msg_send![ns_window, setContentSize: NSSize::new(logical.width, logical.height)];

            // NSVisualEffectView provides the frosted-glass adaptive background.
            // BehindWindow blending works because the window is non-opaque (clearColor).
            let frame = NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(logical.width, logical.height),
            );
            let vev: id = msg_send![class!(NSVisualEffectView), alloc];
            let vev: id = msg_send![vev, initWithFrame: frame];
            // NSVisualEffectMaterialPopover (6): adapts to dark/light mode
            let _: () = msg_send![vev, setMaterial: 6i64];
            // NSVisualEffectBlendingModeBehindWindow (0): blends with desktop content
            let _: () = msg_send![vev, setBlendingMode: 0i64];
            // NSVisualEffectStateActive (1): always render as active
            let _: () = msg_send![vev, setState: 1i64];
            // Resize with parent (NSViewWidthSizable|NSViewHeightSizable = 2|16)
            let _: () = msg_send![vev, setAutoresizingMask: 18i64];

            ns_view.addSubview_(vev);
            ns_view.addSubview_(content.view);
            setup_window(ns_window, ns_view);
        };

        let popup = Self {
            window,
            content,
            cursor_on_separate_monitor: false,
        };
        Ok(popup)
    }

    fn get_size(scale: f64) -> WindowSize {
        LogicalSize::new(300., 52.).to_physical(scale)
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
        trace!("update_with_camera mic={} cam={}", mic_muted, camera_muted);
        self.window.set_title(get_mute_title_text(mic_muted));
        self.update_placement()?;
        self.content.update(mic_muted, camera_muted, self.get_theme(), active_device_name)?;
        if mic_muted {
            trace!("Popup calling set_visible(true)");
            self.window.set_visible(true);
            unsafe {
                let ns_window = self.window.ns_window() as id;
                trace!("Popup ns_window ptr: {:p}", ns_window);
                let _: () = msg_send![ns_window, orderFrontRegardless];
                trace!("Popup orderFrontRegardless called");
            }
        }
        Ok(self)
    }

    pub fn hide(&mut self) -> Result<&mut Self> {
        self.window.set_visible(false);
        Ok(self)
    }

    pub fn update_placement(&mut self) -> Result<&mut Self> {
        let monitor = self.get_current_monitor()?
            .or_else(|| self.window.primary_monitor());
        if let Some(monitor) = monitor {
            let scale = monitor.scale_factor();
            let size = Popup::get_size(scale);
            self.cursor_on_separate_monitor = false;

            // Compute position in tao's logical coordinate system (y=0 at top of primary, downward).
            let monitor_pos = monitor.position().cast::<f64>();
            let monitor_size = monitor.size().cast::<f64>();
            let win_l: LogicalSize<f64> = size.to_logical(scale);
            let mon_pos_l_x = monitor_pos.x / scale;
            let mon_pos_l_y = monitor_pos.y / scale;
            let mon_size_l_w = monitor_size.width / scale;
            let mon_size_l_h = monitor_size.height / scale;
            let tao_x = mon_pos_l_x + (mon_size_l_w / 2.) - (win_l.width / 2.);
            let tao_y = mon_pos_l_y + mon_size_l_h - win_l.height * 2.;

            // tao's set_outer_position clamps negative y values (monitors above the primary),
            // so position the window directly via NSWindow using NSScreen coordinates.
            // NSScreen has y=0 at the bottom of the primary display, increasing upward.
            // Conversion: ns_y = primaryHeight - (tao_y + win_height)
            unsafe {
                let ns_window = self.window.ns_window() as id;
                let main_screen: id = msg_send![class!(NSScreen), mainScreen];
                let screen_frame: NSRect = msg_send![main_screen, frame];
                let primary_h = screen_frame.size.height;
                let ns_x = tao_x;
                let ns_y = primary_h - (tao_y + win_l.height);

                // Set the window size directly, bypassing tao's set_inner_size which passes
                // physical pixel values and can confuse NSWindow (points vs pixels).
                let _: () = msg_send![ns_window, setContentSize: NSSize::new(win_l.width, win_l.height)];
                let _: () = msg_send![ns_window, setFrameOrigin: NSPoint::new(ns_x, ns_y)];

                let actual: NSRect = msg_send![ns_window, frame];
                trace!(
                    "Popup placement: monitor={:?} scale={} tao=({:.0},{:.0}) ns=({:.0},{:.0}) size={:?} actual=({:.0},{:.0} {:.0}x{:.0})",
                    monitor.name(), scale, tao_x, tao_y, ns_x, ns_y, win_l,
                    actual.origin.x, actual.origin.y, actual.size.width, actual.size.height
                );
            }
        } else {
            trace!("Popup placement: no monitor found");
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

}
