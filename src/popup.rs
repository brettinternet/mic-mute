use crate::event_loop::EventLoopTarget;
use crate::feedback::MicUpdate;
use crate::popup_content::{PopupContent, PopupState};
use crate::settings::{PopupPosition, PopupSize};
use crate::utils::get_cursor_pos;
use anyhow::{Context, Result};
use log::trace;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSScreen, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility};
use objc2_foundation::NSRect;
use std::time::{Duration, Instant};
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    monitor::MonitorHandle,
    platform::macos::{MonitorHandleExtMacOS, WindowBuilderExtMacOS, WindowExtMacOS},
    window::{Theme, Window, WindowBuilder},
};

const MUTED_TITLE: &str = "Muted";
const UNMUTED_TITLE: &str = "Unmuted";

/// The medium popup's width and height, which it has always had; `window_size` scales them.
const MEDIUM_WIDTH: f64 = 250.;
const MEDIUM_HEIGHT: f64 = 40.;
/// The gap between the popup and the monitor's edges, or the menu bar: the medium popup's
/// height, the gap it has always had above the bottom edge.
const EDGE_MARGIN: f64 = 40.;

pub type WindowSize<T = f64> = LogicalSize<T>;

/// Where the popup sits and how large it is (`popup_position`, `popup_size`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PopupLayout {
    pub position: PopupPosition,
    pub size: PopupSize,
}

/// How large a popup of `size` is, the medium size's multiple. Its text and icons scale with it.
pub fn size_scale(size: PopupSize) -> f64 {
    match size {
        PopupSize::Small => 0.75,
        PopupSize::Medium => 1.,
        PopupSize::Large => 1.5,
    }
}

fn window_size(size: PopupSize) -> WindowSize {
    let scale = size_scale(size);
    LogicalSize::new(MEDIUM_WIDTH * scale, MEDIUM_HEIGHT * scale)
}

/// A monitor's frame in tao's logical coordinates (points, y growing down from the main
/// display's top edge), and how far its menu bar reaches down.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MonitorArea {
    position: LogicalPosition<f64>,
    size: LogicalSize<f64>,
    /// 0 on a monitor without a menu bar, or while it hides.
    menu_bar_height: f64,
}

/// The popup's top-left corner for `position` on `area`: `EDGE_MARGIN` from the monitor's
/// edges, and below the menu bar at the top. The Dock doesn't move it; the popup ignores the
/// mouse, so it can't block a click on the Dock.
fn popup_origin(
    area: MonitorArea,
    window: WindowSize,
    position: PopupPosition,
) -> LogicalPosition<f64> {
    let left = area.position.x + EDGE_MARGIN;
    let center = area.position.x + (area.size.width - window.width) / 2.;
    let right = area.position.x + area.size.width - EDGE_MARGIN - window.width;
    let top = area.position.y + area.menu_bar_height + EDGE_MARGIN;
    let bottom = area.position.y + area.size.height - EDGE_MARGIN - window.height;
    let (x, y) = match position {
        PopupPosition::TopLeft => (left, top),
        PopupPosition::TopCenter => (center, top),
        PopupPosition::TopRight => (right, top),
        PopupPosition::BottomLeft => (left, bottom),
        PopupPosition::BottomCenter => (center, bottom),
        PopupPosition::BottomRight => (right, bottom),
    };
    LogicalPosition::new(x, y)
}

/// How far a screen's menu bar reaches down: from the top of its `frame` to the top of its
/// `visible_frame`, both AppKit rects (y growing up). 0 for anything odd, so a bad rect can
/// only move the popup up to the edge margin.
fn menu_bar_height(frame: NSRect, visible_frame: NSRect) -> f64 {
    let top = frame.origin.y + frame.size.height;
    let visible_top = visible_frame.origin.y + visible_frame.size.height;
    let height = top - visible_top;
    if height.is_finite() {
        height.max(0.)
    } else {
        0.
    }
}

/// `monitor`'s menu bar height from its `NSScreen`; 0 when AppKit has no screen for it.
fn screen_menu_bar_height(monitor: &MonitorHandle, _mtm: MainThreadMarker) -> f64 {
    // SAFETY: `ns_screen` hands over a reference to an NSScreen (`Retained::into_raw`), which
    // `from_raw` takes and drops after the read; `_mtm` shows this runs on the main thread.
    let screen = monitor
        .ns_screen()
        .and_then(|screen| unsafe { Retained::from_raw(screen.cast::<NSScreen>()) });
    screen.map_or(0., |screen| {
        menu_bar_height(screen.frame(), screen.visibleFrame())
    })
}

fn monitor_area(monitor: &MonitorHandle, mtm: MainThreadMarker) -> MonitorArea {
    let scale = monitor.scale_factor();
    MonitorArea {
        position: monitor.position().to_logical::<f64>(scale),
        size: monitor.size().to_logical::<f64>(scale),
        menu_bar_height: screen_menu_bar_height(monitor, mtm),
    }
}

/// Where a popup with `layout` goes on `monitor`.
fn position_on(
    monitor: &MonitorHandle,
    layout: PopupLayout,
    mtm: MainThreadMarker,
) -> LogicalPosition<f64> {
    popup_origin(
        monitor_area(monitor, mtm),
        window_size(layout.size),
        layout.position,
    )
}

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

/// Whether the popup stays up on its own, outside any flash: muted with the popup on, unless it
/// hides after each mute (`popup_mic_off_ms`), when only a flash shows it.
fn should_pin_popup(enabled: bool, muted: bool, flashes: PopupFlashes) -> bool {
    should_show_popup(enabled, muted) && flashes.keeps_up_while_muted()
}

/// How long the popup shows each state after a request changes it: "Mic on" after an unmute
/// (`popup_mic_on_ms`), and "Mic off" after a mute (`popup_mic_off_ms`), which `None` keeps up
/// for as long as the microphones stay muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupFlashes {
    pub mic_on: Duration,
    pub mic_off: Option<Duration>,
}

impl PopupFlashes {
    /// Whether the popup stays up while muted, rather than hiding after each mute.
    fn keeps_up_while_muted(self) -> bool {
        self.mic_off.is_none()
    }
}

/// When the popup's flashes end: the latest request's, and a `status` URL's. Once the later one
/// is over (`end_due`), the popup hides unless it is pinned; until then the event loop folds
/// `hide_at` into its `WaitUntil`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlashDeadlines {
    /// The latest request's flash: "Mic off" after a mute while the popup hides after each mute,
    /// "Mic on" after an unmute. Each request replaces it, so a mute during "Mic on" restarts
    /// the mute display, and an unmute ends "Mic off".
    request: Option<Instant>,
    /// A `status` URL's flash: a later one extends it, and requests never cut it short.
    status: Option<Instant>,
}

impl FlashDeadlines {
    /// Starts the flash for `update`, which took the microphones from `was_muted` to `muted` at
    /// `now`, timed by `flashes`. Returns whether the flash shows the popup, which an enabled
    /// popup then does:
    ///
    /// - A request that leaves them muted flashes "Mic off" for `flashes.mic_off`, from the
    ///   start each time, or, while the popup stays up while muted, ends the request's flash:
    ///   a pinned popup needs none.
    /// - One that leaves them unmuted flashes "Mic on" for `flashes.mic_on`. It shows a hidden
    ///   popup only when it unmuted and the popup hides after each mute: one that stays up while
    ///   muted is up already, and an unmute that changed nothing only extends a flash on screen.
    /// - A 0 ms flash shows nothing and ends in the same pass.
    /// - The poll's re-mutes (`MicUpdate::Enforcement`) show nothing and change no deadline.
    fn on_mic_update(
        &mut self,
        update: MicUpdate,
        was_muted: bool,
        muted: bool,
        flashes: PopupFlashes,
        now: Instant,
    ) -> bool {
        if update == MicUpdate::Enforcement {
            return false;
        }
        let duration = if muted {
            flashes.mic_off
        } else {
            Some(flashes.mic_on)
        };
        // A deadline `Instant` can't hold ends the flash at once rather than never.
        self.request = duration.map(|duration| now.checked_add(duration).unwrap_or(now));
        let can_show = muted || (was_muted && !flashes.keeps_up_while_muted());
        can_show && duration.is_some_and(|duration| !duration.is_zero())
    }

    /// Keeps the popup up until at least `until`, for a `status` URL.
    fn on_status(&mut self, until: Instant) {
        self.status = self.status.max(Some(until));
    }

    /// When the last flash ends; `None` while there is none.
    fn hide_at(&self) -> Option<Instant> {
        self.request.max(self.status)
    }

    /// Ends the flashes once the last is over at `now`. Returns whether they ended, so the popup
    /// hides unless it is pinned.
    fn end_due(&mut self, now: Instant) -> bool {
        let is_over = self.hide_at().is_some_and(|deadline| now >= deadline);
        if is_over {
            *self = Self::default();
        }
        is_over
    }
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

/// `window`'s `NSWindow`, which lives as long as `window`.
fn ns_window(window: &Window, _mtm: MainThreadMarker) -> &NSWindow {
    // SAFETY: tao's pointer is the window's NSWindow, never null, and valid until `window`
    // drops; `_mtm` shows this runs on the main thread, where AppKit objects may be used.
    unsafe { &*window.ns_window().cast::<NSWindow>() }
}

fn setup_window(window: &NSWindow) {
    window.setHasShadow(true);
    // Rounded edges hack: https://stackoverflow.com/a/37418915
    window.setStyleMask(
        window.styleMask() | NSWindowStyleMask::Titled | NSWindowStyleMask::FullSizeContentView,
    );
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setTitlebarAppearsTransparent(true);
}

pub struct Popup {
    window: Window,
    content: PopupContent,
    current_monitor: Option<MonitorHandle>,
    enabled: bool,
    /// Where the popup sits and how large it is; `content` is drawn for this size.
    layout: PopupLayout,
    /// How long it shows "Mic on" and "Mic off" after a request.
    flashes: PopupFlashes,
    /// When the flashes on screen end.
    deadlines: FlashDeadlines,
    /// The latest state from `update_with_camera`.
    mic_muted: bool,
    camera_in_use: bool,
    /// What the title and content show; `redraw` leaves them alone while this is current.
    shown: PopupState,
    /// The popup lives on the main thread, where its AppKit calls run.
    mtm: MainThreadMarker,
}

impl Popup {
    pub fn new(
        target: &EventLoopTarget,
        mic_muted: bool,
        enabled: bool,
        layout: PopupLayout,
        flashes: PopupFlashes,
    ) -> Result<Self> {
        let mtm =
            MainThreadMarker::new().context("The popup must be created on the main thread")?;
        // The camera state arrives with the first update; until then the content says
        // "Camera on", as it always has.
        let camera_in_use = true;
        let initial_monitor = Popup::get_initial_monitor(target);
        let size = window_size(layout.size);
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
            builder = builder.with_position(position_on(monitor, layout, mtm));
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
        let content = PopupContent::new(mtm, shown, size, size_scale(layout.size))?;
        let ns_window = ns_window(&window, mtm);
        ns_window
            .contentView()
            .context("The popup window has no content view")?
            .addSubview(&content.view);
        setup_window(ns_window);

        let popup = Self {
            window,
            content,
            current_monitor: initial_monitor,
            enabled,
            layout,
            flashes,
            deadlines: FlashDeadlines::default(),
            mic_muted,
            camera_in_use,
            shown,
            mtm,
        };
        Ok(popup)
    }

    pub fn get_theme(&self) -> Theme {
        self.window.theme()
    }

    /// Redraws what changed, then brings the popup to the front whenever it is pinned
    /// (`should_pin_popup`); the front-ordering runs on every call, not only on a change. Only a
    /// request's flash (`on_mic_update`) shows a popup that hides after each mute: for one, the
    /// poll's re-mutes and camera changes only redraw. `camera_muted` is
    /// `CameraController::muted` (`true` while no camera is in use).
    pub fn update_with_camera(&mut self, mic_muted: bool, camera_muted: bool) -> Result<&mut Self> {
        self.mic_muted = mic_muted;
        self.camera_in_use = !camera_muted;
        self.update_placement()?;
        self.redraw()?;
        if self.is_pinned() {
            self.show_front();
        }
        Ok(self)
    }

    /// Starts the flash for `update`, which took the microphones from `was_muted` to `muted` at
    /// `now` (`FlashDeadlines::on_mic_update`), and shows the popup when the flash does. Runs
    /// after `update_with_camera`, which drew the new state and placed the popup. A disabled
    /// popup shows no flash, so it schedules none: nothing then follows the cursor for it.
    pub fn on_mic_update(&mut self, update: MicUpdate, was_muted: bool, muted: bool, now: Instant) {
        if !self.enabled {
            return;
        }
        if self
            .deadlines
            .on_mic_update(update, was_muted, muted, self.flashes, now)
        {
            self.show_front();
        }
    }

    /// When the popup's flashes end and it hides, unless it is pinned; `None` while there is no
    /// flash.
    pub fn hide_at(&self) -> Option<Instant> {
        self.deadlines.hide_at()
    }

    /// Ends the flashes once the last is over at `now`, hiding the popup unless it is pinned.
    pub fn end_flash(&mut self, now: Instant) -> Result<&mut Self> {
        if self.deadlines.end_due(now) {
            self.hide_unless_pinned()?;
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

    /// Shows the popup with the current state until at least `until`, even while it is disabled
    /// or the mic is unmuted: a `status` URL asks for it. `end_flash` then hides it, unless it
    /// is pinned.
    pub fn show_status(&mut self, until: Instant) -> Result<&mut Self> {
        self.deadlines.on_status(until);
        self.update_placement()?;
        self.redraw()?;
        self.order_front();
        Ok(self)
    }

    /// Hides the popup unless it is pinned: muted, enabled, and kept up while muted. A popup
    /// that hides after each mute never stays.
    fn hide_unless_pinned(&mut self) -> Result<&mut Self> {
        if !self.is_pinned() {
            self.hide()?;
        }
        Ok(self)
    }

    /// Whether the popup stays up on its own (`should_pin_popup`).
    fn is_pinned(&self) -> bool {
        should_pin_popup(self.enabled, self.mic_muted, self.flashes)
    }

    /// Times the popup's flashes with `flashes`. Does nothing for the durations in use, so
    /// `UI::apply_settings` can repeat it. A flash on screen keeps the end it had. A switch to
    /// hiding after each mute hides a popup kept up while muted at once, or, during a flash,
    /// when the flash ends; a switch back shows the popup while muted.
    pub fn set_flashes(&mut self, flashes: PopupFlashes) -> Result<&mut Self> {
        if flashes == self.flashes {
            return Ok(self);
        }
        let was_kept_up = self.flashes.keeps_up_while_muted();
        self.flashes = flashes;
        trace!("Popup flashes {:?}", flashes);
        if flashes.keeps_up_while_muted() == was_kept_up {
            return Ok(self);
        }
        if self.is_pinned() {
            self.show_front();
        } else if self.deadlines.hide_at().is_none() {
            self.hide()?;
        }
        Ok(self)
    }

    /// Moves and resizes the popup for `layout`, redrawing its content at the new size. Does
    /// nothing for the layout in use, so `UI::apply_settings` can repeat it.
    pub fn set_layout(&mut self, layout: PopupLayout) -> Result<&mut Self> {
        if layout == self.layout {
            return Ok(self);
        }
        if layout.size != self.layout.size {
            self.replace_content(layout.size)?;
        }
        self.layout = layout;
        trace!("Popup layout {:?}", layout);
        self.update_placement()?;
        Ok(self)
    }

    /// Swaps the content for one drawn for `size`, showing what the old one showed. On an error
    /// the old content stays.
    fn replace_content(&mut self, size: PopupSize) -> Result<()> {
        let content = PopupContent::new(self.mtm, self.shown, window_size(size), size_scale(size))
            .context("Failed to draw the popup at its new size")?;
        let content_view = ns_window(&self.window, self.mtm)
            .contentView()
            .context("The popup window has no content view")?;
        // The content view holds the old stack view; once it's removed, dropping the old
        // content frees it and its subviews.
        self.content.view.removeFromSuperview();
        content_view.addSubview(&content.view);
        self.content = content;
        Ok(())
    }

    /// Enabling the popup while the mic is muted shows it at once, unless it hides after each
    /// mute: then the next request's flash shows it. Disabling it hides it.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<&mut Self> {
        self.enabled = enabled;
        if !enabled {
            self.hide()?;
        } else if self.is_pinned() {
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

            self.window.set_inner_size(window_size(self.layout.size));
            self.window
                .set_outer_position(position_on(&monitor, self.layout, self.mtm));
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
        ns_window(&self.window, self.mtm).orderFrontRegardless();
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

    /// A 1440×900 main display with a 24 pt menu bar.
    const MAIN_DISPLAY: MonitorArea = MonitorArea {
        position: LogicalPosition { x: 0., y: 0. },
        size: LogicalSize {
            width: 1440.,
            height: 900.,
        },
        menu_bar_height: 24.,
    };

    fn origin(area: MonitorArea, position: PopupPosition, size: PopupSize) -> (f64, f64) {
        let origin = popup_origin(area, window_size(size), position);
        (origin.x, origin.y)
    }

    #[test]
    fn the_default_layout_keeps_the_popup_where_it_has_always_been() {
        let layout = PopupLayout::default();
        assert_eq!(layout.position, PopupPosition::BottomCenter);
        assert_eq!(window_size(layout.size), LogicalSize::new(250., 40.));
        // Centered, one popup height above the bottom edge, as before the setting existed.
        let (x, y) = origin(MAIN_DISPLAY, layout.position, layout.size);
        assert_eq!((x, y), (1440. / 2. - 250. / 2., 900. - 40. * 2.));
    }

    #[test]
    fn window_sizes_scale_the_medium_size() {
        let sizes = PopupSize::ALL.map(window_size);
        assert_eq!(
            sizes,
            [
                LogicalSize::new(187.5, 30.),
                LogicalSize::new(250., 40.),
                LogicalSize::new(375., 60.)
            ]
        );
    }

    #[test]
    fn each_position_keeps_the_edge_margin_on_the_main_display() {
        use PopupPosition::*;
        use PopupSize::*;
        let cases = [
            (TopLeft, Medium, (40., 64.)),
            (TopCenter, Medium, (595., 64.)),
            (TopRight, Medium, (1150., 64.)),
            (BottomLeft, Medium, (40., 820.)),
            (BottomCenter, Medium, (595., 820.)),
            (BottomRight, Medium, (1150., 820.)),
            (TopLeft, Small, (40., 64.)),
            (TopCenter, Small, (626.25, 64.)),
            (BottomRight, Small, (1212.5, 830.)),
            (TopRight, Large, (1025., 64.)),
            (BottomCenter, Large, (532.5, 800.)),
            (BottomLeft, Large, (40., 800.)),
        ];
        for (position, size, expected) in cases {
            assert_eq!(
                origin(MAIN_DISPLAY, position, size),
                expected,
                "{position:?} {size:?}"
            );
        }
    }

    #[test]
    fn every_layout_fits_inside_the_monitor() {
        for size in PopupSize::ALL {
            let window = window_size(size);
            for position in PopupPosition::ALL {
                let (x, y) = origin(MAIN_DISPLAY, position, size);
                assert!(x >= EDGE_MARGIN, "{position:?} {size:?}");
                assert!(
                    x + window.width <= 1440. - EDGE_MARGIN,
                    "{position:?} {size:?}"
                );
                assert!(y >= 24. + EDGE_MARGIN, "{position:?} {size:?}");
                assert!(
                    y + window.height <= 900. - EDGE_MARGIN,
                    "{position:?} {size:?}"
                );
            }
        }
    }

    #[test]
    fn a_secondary_monitor_places_the_popup_on_its_own_frame() {
        use PopupPosition::*;
        // Left of and above the main display, without a menu bar.
        let secondary = MonitorArea {
            position: LogicalPosition::new(-1920., -300.),
            size: LogicalSize::new(1920., 1080.),
            menu_bar_height: 0.,
        };
        let cases = [
            (TopLeft, (-1880., -260.)),
            (TopCenter, (-1085., -260.)),
            (TopRight, (-290., -260.)),
            (BottomLeft, (-1880., 700.)),
            (BottomCenter, (-1085., 700.)),
            (BottomRight, (-290., 700.)),
        ];
        for (position, expected) in cases {
            assert_eq!(
                origin(secondary, position, PopupSize::Medium),
                expected,
                "{position:?}"
            );
        }
    }

    #[test]
    fn top_positions_sit_below_a_notched_menu_bar() {
        let notched = MonitorArea {
            menu_bar_height: 37.,
            ..MAIN_DISPLAY
        };
        for position in [
            PopupPosition::TopLeft,
            PopupPosition::TopCenter,
            PopupPosition::TopRight,
        ] {
            let (_, y) = origin(notched, position, PopupSize::Medium);
            assert_eq!(y, 37. + EDGE_MARGIN, "{position:?}");
        }
        let (_, bottom) = origin(notched, PopupPosition::BottomCenter, PopupSize::Medium);
        assert_eq!(
            bottom, 820.,
            "the menu bar doesn't move the bottom positions"
        );
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        use objc2_foundation::{NSPoint, NSSize};
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    #[test]
    fn menu_bar_height_is_the_gap_between_the_frames_tops() {
        // AppKit rects grow up: a 24 pt menu bar on top, a 70 pt Dock at the bottom.
        let frame = rect(0., 0., 1440., 900.);
        assert_eq!(menu_bar_height(frame, rect(0., 70., 1440., 806.)), 24.);
        // A display above the main one, without a menu bar.
        let above = rect(0., 900., 1920., 1080.);
        assert_eq!(menu_bar_height(above, above), 0.);
        // Odd rects never move the popup above the edge margin.
        assert_eq!(menu_bar_height(frame, rect(0., 0., 1440., 950.)), 0.);
        assert_eq!(menu_bar_height(frame, rect(0., f64::NAN, 1440., 876.)), 0.);
    }

    #[test]
    fn popup_visibility_requires_enabled_setting_and_mute() {
        assert!(should_show_popup(true, true));
        assert!(!should_show_popup(false, true));
        assert!(!should_show_popup(true, false));
    }

    const MIC_ON: Duration = Duration::from_millis(1_000);
    const MIC_OFF: Duration = Duration::from_millis(1_500);

    /// The popup stays up while muted (`popup_mic_off_ms` null), as it always did.
    const KEPT_UP: PopupFlashes = PopupFlashes {
        mic_on: MIC_ON,
        mic_off: None,
    };
    /// The popup hides `MIC_OFF` after each mute.
    const TRANSIENT: PopupFlashes = PopupFlashes {
        mic_on: MIC_ON,
        mic_off: Some(MIC_OFF),
    };

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn only_a_popup_kept_up_while_muted_is_pinned() {
        assert!(should_pin_popup(true, true, KEPT_UP));
        assert!(
            !should_pin_popup(true, true, TRANSIENT),
            "hides after a mute"
        );
        for flashes in [KEPT_UP, TRANSIENT] {
            assert!(!should_pin_popup(false, true, flashes), "{flashes:?}");
            assert!(!should_pin_popup(true, false, flashes), "{flashes:?}");
            assert!(!should_pin_popup(false, false, flashes), "{flashes:?}");
        }
    }

    #[test]
    fn a_mute_flashes_mic_off_when_the_popup_hides_after_each_mute() {
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();

        let shows = deadlines.on_mic_update(MicUpdate::Request, false, true, TRANSIENT, now);

        assert!(shows);
        assert_eq!(deadlines.hide_at(), Some(now + MIC_OFF));
        assert!(!deadlines.end_due(now + MIC_OFF - ms(1)), "still on");
        assert!(deadlines.end_due(now + MIC_OFF), "over, so the popup hides");
        assert_eq!(deadlines.hide_at(), None);
    }

    #[test]
    fn a_mute_while_muted_shows_mic_off_again() {
        // A mute URL, or a lock trigger, after the last "Mic off" ended.
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();

        assert!(deadlines.on_mic_update(MicUpdate::Request, true, true, TRANSIENT, now));
        assert_eq!(deadlines.hide_at(), Some(now + MIC_OFF));
    }

    #[test]
    fn a_popup_kept_up_while_muted_gets_no_mute_flash() {
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();
        // An unmute's "Mic on" is on when the mute comes.
        assert!(!deadlines.on_mic_update(MicUpdate::Request, true, false, KEPT_UP, now));
        assert_eq!(deadlines.hide_at(), Some(now + MIC_ON));

        let shows =
            deadlines.on_mic_update(MicUpdate::Request, false, true, KEPT_UP, now + ms(300));

        assert!(!shows, "`update_with_camera` shows a pinned popup");
        assert_eq!(deadlines.hide_at(), None, "pinned, it needs no deadline");
    }

    #[test]
    fn the_polls_re_mutes_never_show_the_popup_or_move_a_deadline() {
        let now = Instant::now();
        for flashes in [KEPT_UP, TRANSIENT] {
            let mut deadlines = FlashDeadlines::default();
            for (was_muted, muted) in [(true, true), (false, true), (true, false), (false, false)] {
                let shows =
                    deadlines.on_mic_update(MicUpdate::Enforcement, was_muted, muted, flashes, now);
                assert!(!shows, "{flashes:?} {was_muted} → {muted}");
                assert_eq!(
                    deadlines.hide_at(),
                    None,
                    "{flashes:?} {was_muted} → {muted}"
                );
            }
        }

        // Nor does a re-mute during a mute's flash restart it.
        let mut deadlines = FlashDeadlines::default();
        deadlines.on_mic_update(MicUpdate::Request, false, true, TRANSIENT, now);
        let later = now + ms(1_000);
        assert!(!deadlines.on_mic_update(MicUpdate::Enforcement, true, true, TRANSIENT, later));
        assert_eq!(deadlines.hide_at(), Some(now + MIC_OFF));
    }

    #[test]
    fn muting_again_restarts_the_mute_display() {
        let now = Instant::now();
        let again = now + ms(400);
        let mut deadlines = FlashDeadlines::default();
        deadlines.on_mic_update(MicUpdate::Request, false, true, TRANSIENT, now);

        assert!(deadlines.on_mic_update(MicUpdate::Request, true, true, TRANSIENT, again));

        assert_eq!(deadlines.hide_at(), Some(again + MIC_OFF));
        assert!(
            !deadlines.end_due(now + MIC_OFF),
            "the first mute's end passed"
        );
    }

    #[test]
    fn a_mute_during_mic_on_ends_it_and_shows_mic_off_for_its_own_time() {
        let flashes = PopupFlashes {
            mic_on: ms(5_000),
            mic_off: Some(ms(500)),
        };
        let now = Instant::now();
        let mute = now + ms(1_000);
        let mut deadlines = FlashDeadlines::default();
        assert!(deadlines.on_mic_update(MicUpdate::Request, true, false, flashes, now));

        assert!(deadlines.on_mic_update(MicUpdate::Request, false, true, flashes, mute));

        assert_eq!(
            deadlines.hide_at(),
            Some(mute + ms(500)),
            "not the 5 s of Mic on"
        );
    }

    #[test]
    fn an_unmute_ends_mic_off_and_shows_mic_on_for_its_own_time() {
        let flashes = PopupFlashes {
            mic_on: ms(200),
            mic_off: Some(ms(5_000)),
        };
        let now = Instant::now();
        let unmute = now + ms(1_000);
        let mut deadlines = FlashDeadlines::default();
        deadlines.on_mic_update(MicUpdate::Request, false, true, flashes, now);

        assert!(deadlines.on_mic_update(MicUpdate::Request, true, false, flashes, unmute));

        assert_eq!(deadlines.hide_at(), Some(unmute + ms(200)));
    }

    #[test]
    fn an_unmute_shows_a_hidden_popup_only_when_it_hides_after_each_mute() {
        let now = Instant::now();
        let mut transient = FlashDeadlines::default();
        assert!(
            transient.on_mic_update(MicUpdate::Request, true, false, TRANSIENT, now),
            "the mute's flash may be over, so Mic on shows the popup again"
        );
        assert_eq!(transient.hide_at(), Some(now + MIC_ON));

        let mut kept_up = FlashDeadlines::default();
        assert!(
            !kept_up.on_mic_update(MicUpdate::Request, true, false, KEPT_UP, now),
            "kept up while muted, the popup already shows; a disabled one stays hidden"
        );
        assert_eq!(kept_up.hide_at(), Some(now + MIC_ON), "Mic on still ends");
    }

    #[test]
    fn an_unmute_that_changed_nothing_only_extends_mic_on() {
        let now = Instant::now();
        let again = now + ms(600);
        for flashes in [KEPT_UP, TRANSIENT] {
            let mut deadlines = FlashDeadlines::default();
            deadlines.on_mic_update(MicUpdate::Request, true, false, flashes, now);

            let shows = deadlines.on_mic_update(MicUpdate::Request, false, false, flashes, again);

            assert!(!shows, "{flashes:?}");
            assert_eq!(deadlines.hide_at(), Some(again + MIC_ON), "{flashes:?}");
        }
    }

    #[test]
    fn zero_ms_flashes_show_nothing_and_end_in_the_same_pass() {
        let flashes = PopupFlashes {
            mic_on: Duration::ZERO,
            mic_off: Some(Duration::ZERO),
        };
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();

        assert!(!deadlines.on_mic_update(MicUpdate::Request, false, true, flashes, now));
        assert!(deadlines.end_due(now));
        assert!(!deadlines.on_mic_update(MicUpdate::Request, true, false, flashes, now));
        assert!(deadlines.end_due(now));
        assert_eq!(deadlines.hide_at(), None);
    }

    #[test]
    fn a_status_flash_lasts_its_time_through_mutes_and_unmutes() {
        let now = Instant::now();
        let status_end = now + ms(2_000);
        let quick = PopupFlashes {
            mic_on: Duration::ZERO,
            mic_off: Some(ms(300)),
        };
        let mut deadlines = FlashDeadlines::default();
        deadlines.on_status(status_end);

        deadlines.on_mic_update(MicUpdate::Request, false, true, quick, now + ms(500));
        assert_eq!(deadlines.hide_at(), Some(status_end));
        deadlines.on_mic_update(MicUpdate::Request, true, false, quick, now + ms(600));
        assert_eq!(deadlines.hide_at(), Some(status_end));

        // A mute's flash that outlasts it keeps the popup up longer.
        deadlines.on_mic_update(MicUpdate::Request, false, true, TRANSIENT, now + ms(1_900));
        assert_eq!(deadlines.hide_at(), Some(now + ms(1_900) + MIC_OFF));
        assert!(!deadlines.end_due(status_end));
    }

    #[test]
    fn a_later_status_extends_its_flash_and_an_earlier_one_keeps_it() {
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();

        deadlines.on_status(now + ms(2_000));
        deadlines.on_status(now + ms(3_000));
        assert_eq!(deadlines.hide_at(), Some(now + ms(3_000)));
        deadlines.on_status(now + ms(2_500));
        assert_eq!(deadlines.hide_at(), Some(now + ms(3_000)));
    }

    #[test]
    fn the_flashes_end_once_the_last_is_over_and_only_once() {
        let now = Instant::now();
        let mut deadlines = FlashDeadlines::default();
        assert!(!deadlines.end_due(now), "no flash, nothing to end");

        deadlines.on_mic_update(MicUpdate::Request, true, false, TRANSIENT, now);
        deadlines.on_status(now + ms(2_000));

        assert!(
            !deadlines.end_due(now + MIC_ON),
            "the status flash is still on"
        );
        assert!(deadlines.end_due(now + ms(2_000)));
        assert!(!deadlines.end_due(now + ms(2_000)), "already ended");
        assert_eq!(deadlines.hide_at(), None);
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
