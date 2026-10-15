use crate::event_loop::EventLoopTarget;
use crate::popup_content::{PopupContent, PopupState};
use crate::utils::get_cursor_pos;
use anyhow::{Context, Result};
use cocoa::{
    appkit::{NSView, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility},
    base::{id, YES},
};
use log::trace;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    monitor::MonitorHandle,
    platform::macos::{WindowBuilderExtMacOS, WindowExtMacOS},
    window::{Theme, Window, WindowBuilder},
};

const MUTED_TITLE: &str = "Muted";
const UNMUTED_TITLE: &str = "Unmuted";

pub type WindowSize<T = f64> = LogicalSize<T>;

fn get_mute_title_text(muted: bool) -> &'static str {
    if muted {
        MUTED_TITLE
    } else {
        UNMUTED_TITLE
    }
}

fn should_show_popup(enabled: bool, muted: bool) -> bool {
    enabled && muted
}

/// The parts of the popup a redraw from `shown` to `next` must touch.
#[derive(Debug, PartialEq, Eq)]
struct Redraw {
    /// The window title, which names the mic state.
    title: bool,
    /// The labels and icons, which also follow the camera and the theme.
    content: bool,
}

fn redraw_for(shown: PopupState, next: PopupState) -> Redraw {
    Redraw {
        title: shown.mic_muted != next.mic_muted,
        content: shown != next,
    }
}

fn monitor_contains_physical_position(
    position: PhysicalPosition<f64>,
    monitor_position: PhysicalPosition<f64>,
    monitor_size: PhysicalSize<f64>,
) -> bool {
    position.x >= monitor_position.x
        && position.x < monitor_position.x + monitor_size.width
        && position.y >= monitor_position.y
        && position.y < monitor_position.y + monitor_size.height
}

fn setup_window(window: id) {
    unsafe {
        window.setHasShadow_(YES);
        // Rounded edges hack: https://stackoverflow.com/a/37418915
        let mask = window.styleMask()
            | NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask;
        // Pass the NSUInteger itself: cocoa's bitflags struct has no guaranteed C layout.
        let _: () = msg_send![window, setStyleMask: mask.bits()];
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
    current_monitor: Option<MonitorHandle>,
    enabled: bool,
    /// The latest state from `update_with_camera`.
    mic_muted: bool,
    camera_in_use: bool,
    /// What the title and content show; `redraw` leaves them alone while this is current.
    shown: PopupState,
}

impl Popup {
    pub fn new(target: &EventLoopTarget, mic_muted: bool, enabled: bool) -> Result<Self> {
        // The camera state arrives with the first update; until then the content says
        // "Camera on", as it always has.
        let camera_in_use = true;
        let initial_monitor = Popup::get_initial_monitor(target);
        let size = Popup::get_size();
        let scale = initial_monitor
            .as_ref()
            .map_or(1.0, MonitorHandle::scale_factor);
        let mut builder = WindowBuilder::new()
            .with_title(get_mute_title_text(mic_muted))
            .with_titlebar_hidden(true)
            .with_movable_by_window_background(true)
            .with_always_on_top(true)
            .with_closable(false)
            .with_content_protection(true)
            .with_decorations(false)
            .with_inner_size(size)
            .with_maximized(false)
            .with_minimizable(false)
            .with_resizable(false)
            .with_visible_on_all_workspaces(true)
            .with_visible(false)
            .with_has_shadow(true);
        if let Some(monitor) = initial_monitor.as_ref() {
            builder = builder.with_position(Popup::get_position(monitor, size));
        }
        let window = builder.build(target).context("Failed to build window")?;
        window.set_visible(false);
        window.set_ignore_cursor_events(true)?;

        trace!("Window scale factor {}", scale);
        let shown = PopupState {
            mic_muted,
            camera_in_use,
            theme: window.theme(),
        };
        let content = PopupContent::new(shown, size)?;
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
            current_monitor: initial_monitor,
            enabled,
            mic_muted,
            camera_in_use,
            shown,
        };
        Ok(popup)
    }

    fn get_size() -> WindowSize {
        LogicalSize::new(250., 40.)
    }

    pub fn get_theme(&self) -> Theme {
        self.window.theme()
    }

    /// Redraws what changed, then brings the popup to the front whenever it should show; the
    /// front-ordering runs on every call, not only on a change. `camera_muted` is
    /// `CameraController::muted` (`true` while no camera is in use).
    pub fn update_with_camera(&mut self, mic_muted: bool, camera_muted: bool) -> Result<&mut Self> {
        self.mic_muted = mic_muted;
        self.camera_in_use = !camera_muted;
        self.update_placement()?;
        self.redraw()?;
        if should_show_popup(self.enabled, mic_muted) {
            self.show_front();
        }
        Ok(self)
    }

    /// Redraws the title and content for the latest state and the current theme, skipping the
    /// parts that already show it. A failed redraw stays unrecorded, so the next call retries.
    fn redraw(&mut self) -> Result<()> {
        let next = PopupState {
            mic_muted: self.mic_muted,
            camera_in_use: self.camera_in_use,
            theme: self.get_theme(),
        };
        let redraw = redraw_for(self.shown, next);
        if redraw.content {
            self.content.render(next)?;
            trace!("Redrew popup content for {:?}", next);
        }
        if redraw.title {
            self.window.set_title(get_mute_title_text(next.mic_muted));
        }
        self.shown = next;
        Ok(())
    }

    pub fn hide(&mut self) -> Result<&mut Self> {
        self.window.set_visible(false);
        Ok(self)
    }

    /// Shows the popup with the current state, even while it is disabled or the mic is
    /// unmuted, until [`Self::hide_unless_pinned`]: a `status` URL asks for it.
    pub fn show_status(&mut self) -> Result<&mut Self> {
        self.update_placement()?;
        self.redraw()?;
        self.order_front();
        Ok(self)
    }

    /// Ends the "Mic on" flash after an unmute, or a status flash: hides the popup unless it
    /// should stay up, muted with the popup enabled.
    pub fn hide_unless_pinned(&mut self) -> Result<&mut Self> {
        if !should_show_popup(self.enabled, self.mic_muted) {
            self.hide()?;
        }
        Ok(self)
    }

    /// Enabling the popup while the mic is muted shows it at once; disabling it hides it.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<&mut Self> {
        self.enabled = enabled;
        if !enabled {
            self.hide()?;
        } else if should_show_popup(enabled, self.mic_muted) {
            self.show_front();
        }
        Ok(self)
    }

    pub fn update_placement(&mut self) -> Result<&mut Self> {
        if let Some(monitor) = self.get_current_monitor()? {
            let monitor_changed = self.current_monitor.as_ref() != Some(&monitor);
            let was_visible = monitor_changed && self.window.is_visible();
            if was_visible {
                self.window.set_visible(false);
            }

            let size = Popup::get_size();
            self.window.set_inner_size(size);
            self.window
                .set_outer_position(Popup::get_position(&monitor, size));
            self.current_monitor = Some(monitor);

            // A visible popup stays visible on the new monitor, a status flash included.
            if was_visible {
                self.order_front();
            }
        }
        Ok(self)
    }

    /// Follows the cursor's monitor, and redraws after a light/dark switch even when no state
    /// update arrives.
    pub fn detect_cursor_monitor(&mut self) -> Result<&mut Self> {
        self.update_placement()?;
        self.redraw()?;
        Ok(self)
    }

    fn get_current_monitor(&self) -> Result<Option<MonitorHandle>> {
        // CoreGraphics and `Window::monitor_from_point` both use the same global
        // display coordinate space on macOS. Prefer this path over
        // `Window::cursor_position`, which converts through the primary display's
        // scale factor and can misclassify points near monitor boundaries.
        if let Some((x, y)) = get_cursor_pos() {
            if let Some(monitor) = self.window.monitor_from_point(x, y) {
                return Ok(Some(monitor));
            }
        }

        let position = self
            .window
            .cursor_position()
            .context("Failed to read cursor position")?;
        if let Some(monitor) = self.window.monitor_from_point(position.x, position.y) {
            return Ok(Some(monitor));
        }

        Ok(self.monitor_from_physical_position(position))
    }

    fn monitor_from_physical_position(
        &self,
        position: PhysicalPosition<f64>,
    ) -> Option<MonitorHandle> {
        self.window.available_monitors().find(|monitor| {
            monitor_contains_physical_position(
                position,
                monitor.position().cast::<f64>(),
                monitor.size().cast::<f64>(),
            )
        })
    }

    fn get_initial_monitor(target: &EventLoopTarget) -> Option<MonitorHandle> {
        target.primary_monitor()
    }

    fn show_front(&self) {
        if self.enabled {
            self.order_front();
        }
    }

    /// Shows the popup in front of every window, whether or not it is enabled.
    fn order_front(&self) {
        self.window.set_visible(true);
        unsafe {
            let ns_window = self.window.ns_window() as id;
            let _: () = msg_send![ns_window, orderFrontRegardless];
        }
    }

    fn get_position(monitor: &MonitorHandle, window_size: WindowSize) -> LogicalPosition<f64> {
        let scale = monitor.scale_factor();
        let monitor_position = monitor.position().to_logical::<f64>(scale);
        let monitor_size = monitor.size().to_logical::<f64>(scale);
        let x: f64 = (monitor_position.x + (monitor_size.width / 2.)) - (window_size.width / 2.);
        let y: f64 = (monitor_position.y + monitor_size.height) - (window_size.height * 2.);
        LogicalPosition::new(x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_contains_fractional_positions_on_negative_edges() {
        let monitor_position = PhysicalPosition::new(-1920.0, 0.0);
        let monitor_size = PhysicalSize::new(1920.0, 1080.0);

        assert!(monitor_contains_physical_position(
            PhysicalPosition::new(-0.25, 100.0),
            monitor_position,
            monitor_size
        ));
    }

    #[test]
    fn monitor_contains_positions_until_exclusive_far_edges() {
        let monitor_position = PhysicalPosition::new(0.0, 0.0);
        let monitor_size = PhysicalSize::new(1440.0, 900.0);

        assert!(monitor_contains_physical_position(
            PhysicalPosition::new(1439.999, 899.999),
            monitor_position,
            monitor_size
        ));
        assert!(!monitor_contains_physical_position(
            PhysicalPosition::new(1440.0, 899.999),
            monitor_position,
            monitor_size
        ));
        assert!(!monitor_contains_physical_position(
            PhysicalPosition::new(1439.999, 900.0),
            monitor_position,
            monitor_size
        ));
    }
    #[test]
    fn popup_visibility_requires_enabled_setting_and_mute() {
        assert!(should_show_popup(true, true));
        assert!(!should_show_popup(false, true));
        assert!(!should_show_popup(true, false));
    }

    const MUTED_IDLE_DARK: PopupState = PopupState {
        mic_muted: true,
        camera_in_use: false,
        theme: Theme::Dark,
    };

    #[test]
    fn redraw_skips_a_popup_that_already_shows_the_state() {
        let redraw = redraw_for(MUTED_IDLE_DARK, MUTED_IDLE_DARK);
        assert_eq!(
            redraw,
            Redraw {
                title: false,
                content: false
            }
        );
    }

    #[test]
    fn redraw_retitles_and_redraws_when_the_mic_changes() {
        let unmuted = PopupState {
            mic_muted: false,
            ..MUTED_IDLE_DARK
        };
        let redraw = redraw_for(MUTED_IDLE_DARK, unmuted);
        assert_eq!(
            redraw,
            Redraw {
                title: true,
                content: true
            }
        );
    }

    #[test]
    fn redraw_keeps_the_title_when_only_the_camera_or_theme_changes() {
        let camera_on = PopupState {
            camera_in_use: true,
            ..MUTED_IDLE_DARK
        };
        let light = PopupState {
            theme: Theme::Light,
            ..MUTED_IDLE_DARK
        };
        for next in [camera_on, light] {
            assert_eq!(
                redraw_for(MUTED_IDLE_DARK, next),
                Redraw {
                    title: false,
                    content: true
                },
                "{next:?}"
            );
        }
    }
}
