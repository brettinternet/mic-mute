use crate::icons::{self, RenderedIcon};
use anyhow::Result;
use cocoa::appkit::{NSColor, NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc::runtime::Object;
use std::sync::Arc;
use tao::dpi::LogicalSize;
use tao::window::Theme;

const MUTED_DESCRIPTION: &str = "Mic off";
const UNMUTED_DESCRIPTION: &str = "Mic on";
const CAMERA_MUTED_DESCRIPTION: &str = "Camera off";
const CAMERA_UNMUTED_DESCRIPTION: &str = "Camera on";

/// `NSTextAlignmentCenter`, an `NSInteger` (AppKit `NSText.h`). The header picks the value with
/// `TARGET_ABI_USES_IOS_VALUES`, which `TargetConditionals.h` defines as `!TARGET_CPU_X86_64 || …`:
/// Apple silicon Macs use the iOS value 1 (`NSTextAlignmentRight` = 2), Intel Macs use 2
/// (`NSTextAlignmentRight` = 1).
const NS_TEXT_ALIGNMENT_CENTER: i64 = if cfg!(target_arch = "x86_64") { 2 } else { 1 };
/// `NSStackViewGravityCenter = 2`, an `NSInteger` (AppKit `NSStackView.h`).
const NS_STACK_VIEW_GRAVITY_CENTER: i64 = 2;

pub fn get_mic_mute_description_text(muted: bool) -> &'static str {
    if muted {
        MUTED_DESCRIPTION
    } else {
        UNMUTED_DESCRIPTION
    }
}

pub fn get_camera_mute_description_text(muted: bool) -> &'static str {
    if muted {
        CAMERA_MUTED_DESCRIPTION
    } else {
        CAMERA_UNMUTED_DESCRIPTION
    }
}

/// Vertically-centered 18pt-tall rect spanning the full width.
/// Matches the original layout so the NSStackView stays at a fixed size
/// and does not activate Auto Layout resizing on the window.
fn get_frame_rect(size: LogicalSize<f64>) -> NSRect {
    const LINE_HEIGHT: f64 = 18.;
    NSRect::new(
        NSPoint::new(0., (size.height - LINE_HEIGHT) / 2.),
        NSSize::new(size.width, LINE_HEIGHT),
    )
}

fn get_text_color(muted: bool, theme: Theme) -> id {
    unsafe {
        // 239, 68, 68 (light mode red) - #ef4444 / 248, 113, 113 (dark mode red) - #f87171
        let dark_red = NSColor::colorWithRed_green_blue_alpha_(nil, 0.9372, 0.2666, 0.2666, 1.);
        let light_red = NSColor::colorWithRed_green_blue_alpha_(nil, 0.9725, 0.4431, 0.4431, 1.);
        let black = NSColor::colorWithRed_green_blue_alpha_(nil, 0., 0., 0., 1.);
        let white = NSColor::colorWithRed_green_blue_alpha_(nil, 1., 1., 1., 1.);
        match theme {
            Theme::Light if muted => dark_red,
            Theme::Light => black,
            Theme::Dark if muted => light_red,
            _ => white,
        }
    }
}

/// Sets `label`'s text and color.
unsafe fn set_label(label: id, text: &str, color: id) {
    let text = NSString::alloc(nil).init_str(text);
    label.setStringValue_(text);
    let _: () = msg_send![text, release];
    let _: () = msg_send![label, setTextColor: color];
}

fn get_textfield(text: &str, color: id, frame: NSRect) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let label: id = msg_send![label, initWithFrame: frame];
        set_label(label, text, color);
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let _: () = msg_send![label, setAlignment: NS_TEXT_ALIGNMENT_CENTER];
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, systemFontSize];
        let custom_font: *mut Object = msg_send![ns_font, systemFontOfSize: default_size + 3.0_f64];
        let _: () = msg_send![label, setFont: custom_font];
        label
    }
}

/// A new `NSImage` of `icon`'s cached PNG, 16 pt tall; the caller owns it (+1) and releases it.
fn ns_image(icon: &RenderedIcon) -> id {
    const ICON_HEIGHT: f64 = 16.;
    let icon_width = f64::from(icon.width) / (f64::from(icon.height) / ICON_HEIGHT);
    unsafe {
        let nsdata = NSData::dataWithBytes_length_(
            nil,
            icon.png.as_ptr() as *const std::os::raw::c_void,
            icon.png.len() as u64,
        );
        let ns_image = NSImage::initWithData_(NSImage::alloc(nil), nsdata);
        let _: () = msg_send![ns_image, setSize: NSSize::new(icon_width, ICON_HEIGHT)];
        let _: () = msg_send![ns_image, setTemplate: NO];
        ns_image
    }
}

/// Shows `icon` in `image_view`, which then holds the only reference to the new `NSImage`.
unsafe fn set_image(image_view: id, icon: &RenderedIcon) {
    let image = ns_image(icon);
    image_view.setImage_(image);
    let _: () = msg_send![image, release];
}

fn make_image_view(icon: &RenderedIcon, frame: NSRect) -> id {
    unsafe {
        let view = NSImageView::alloc(nil);
        let view: id = msg_send![view, initWithFrame: frame];
        set_image(view, icon);
        view
    }
}

/// The mic and camera icons for `state`, from the process-wide cache.
fn icons_for(state: PopupState) -> Result<(Arc<RenderedIcon>, Arc<RenderedIcon>)> {
    let mic = icons::popup_mic(state.mic_muted, state.theme).rendered()?;
    let camera = icons::popup_camera(state.camera_in_use, state.theme).rendered()?;
    Ok((mic, camera))
}

/// 1pt-wide vertical separator. Explicit size constraints tell the gravity-based
/// NSStackView its width without activating the Auto Layout feedback loop that
/// causes the window to grow.
unsafe fn make_separator_view(line_height: f64) -> id {
    let sep: id = msg_send![class!(NSView), alloc];
    let sep: id = msg_send![sep, initWithFrame: NSRect::new(
        NSPoint::new(0., 0.),
        NSSize::new(1., line_height),
    )];
    let _: () = msg_send![sep, setWantsLayer: YES];
    let layer: id = msg_send![sep, layer];
    let color: id = msg_send![class!(NSColor), colorWithWhite: 0.5_f64 alpha: 0.5_f64];
    let cg_color: *const std::os::raw::c_void = msg_send![color, CGColor];
    let _: () = msg_send![layer, setBackgroundColor: cg_color];
    // Size constraints without position constraints — NSStackView controls position.
    // NSLayoutConstraint.h, all NSInteger: NSLayoutAttributeWidth = 7, NSLayoutAttributeHeight = 8,
    // NSLayoutAttributeNotAnAttribute = 0, NSLayoutRelationEqual = 0.
    let _: () = msg_send![sep, setTranslatesAutoresizingMaskIntoConstraints: NO];
    let w: id = msg_send![class!(NSLayoutConstraint),
        constraintWithItem: sep attribute: 7i64 relatedBy: 0i64
        toItem: nil attribute: 0i64 multiplier: 1.0_f64 constant: 1.0_f64];
    let _: () = msg_send![sep, addConstraint: w];
    let h: id = msg_send![class!(NSLayoutConstraint),
        constraintWithItem: sep attribute: 8i64 relatedBy: 0i64
        toItem: nil attribute: 0i64 multiplier: 1.0_f64 constant: line_height];
    let _: () = msg_send![sep, addConstraint: h];
    sep
}

/// What the popup content shows; `Popup` redraws it only when this changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupState {
    pub mic_muted: bool,
    pub camera_in_use: bool,
    pub theme: Theme,
}

/// Non-owning ids: the stack view `view` (owned by the window's content view) retains the rest.
#[derive(Copy, Clone)]
pub struct PopupContent {
    mic_label: id,
    mic_image: id,
    camera_image: id,
    camera_label: id,
    pub view: id,
}

impl PopupContent {
    pub fn new(state: PopupState, size: LogicalSize<f64>) -> Result<Self> {
        // Fetch the icons before allocating any view, so an error leaks none.
        let (mic_icon, camera_icon) = icons_for(state)?;
        let frame = get_frame_rect(size);

        let mic_label = get_textfield(
            get_mic_mute_description_text(state.mic_muted),
            get_text_color(state.mic_muted, state.theme),
            frame,
        );
        let mic_image = make_image_view(&mic_icon, frame);
        let camera_image = make_image_view(&camera_icon, frame);
        let camera_off = !state.camera_in_use;
        let camera_label = get_textfield(
            get_camera_mute_description_text(camera_off),
            get_text_color(camera_off, state.theme),
            frame,
        );

        let view = unsafe {
            let stack: *mut Object = msg_send![class!(NSStackView), alloc];
            let stack: id = msg_send![stack, initWithFrame: frame];
            let gravity = NS_STACK_VIEW_GRAVITY_CENTER;
            let _: () = msg_send![stack, addView: mic_image inGravity: gravity];
            let _: () = msg_send![mic_image, release];
            let _: () = msg_send![stack, addView: mic_label inGravity: gravity];
            let _: () = msg_send![mic_label, release];
            let sep = make_separator_view(frame.size.height);
            let _: () = msg_send![stack, addView: sep inGravity: gravity];
            let _: () = msg_send![sep, release];
            let _: () = msg_send![stack, addView: camera_image inGravity: gravity];
            let _: () = msg_send![camera_image, release];
            let _: () = msg_send![stack, addView: camera_label inGravity: gravity];
            let _: () = msg_send![camera_label, release];
            stack
        };

        Ok(Self {
            mic_label,
            mic_image,
            camera_image,
            camera_label,
            view,
        })
    }

    /// Shows `state` in the labels and icons. On an error nothing has changed yet.
    pub fn render(&self, state: PopupState) -> Result<()> {
        let (mic_icon, camera_icon) = icons_for(state)?;
        let camera_off = !state.camera_in_use;
        unsafe {
            set_label(
                self.mic_label,
                get_mic_mute_description_text(state.mic_muted),
                get_text_color(state.mic_muted, state.theme),
            );
            set_image(self.mic_image, &mic_icon);
            set_image(self.camera_image, &camera_icon);
            set_label(
                self.camera_label,
                get_camera_mute_description_text(camera_off),
                get_text_color(camera_off, state.theme),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mic_mute_description_muted() {
        assert_eq!(get_mic_mute_description_text(true), "Mic off");
    }

    #[test]
    fn test_mic_mute_description_unmuted() {
        assert_eq!(get_mic_mute_description_text(false), "Mic on");
    }
}
