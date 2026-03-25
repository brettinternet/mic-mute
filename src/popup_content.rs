use crate::icons::{popup_icon_color, rasterize_svg};
use anyhow::{Context, Result};
use cocoa::appkit::{NSColor, NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc::runtime::Object;
use tao::dpi::LogicalSize;
use tao::window::Theme;

const MUTED_DESCRIPTION: &str = "Mic off";
const UNMUTED_DESCRIPTION: &str = "Mic on";
const CAMERA_MUTED_DESCRIPTION: &str = "Camera off";
const CAMERA_UNMUTED_DESCRIPTION: &str = "Camera on";

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

fn get_textfield(text: &str, color: id, frame: NSRect) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: frame];
        let label_str = NSString::alloc(nil).init_str(text);
        label.setStringValue_(label_str);
        let _: () = msg_send![label_str, release];
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        const NSALIGNMENT_CENTER: i32 = 1;
        let _: () = msg_send![label, setAlignment: NSALIGNMENT_CENTER];
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, systemFontSize];
        let custom_font: *mut Object = msg_send![ns_font, systemFontOfSize: default_size + 3.0_f64];
        let _: () = msg_send![label, setFont: custom_font];
        label
    }
}

/// Rasterizes an SVG and returns PNG-encoded bytes plus source dimensions.
/// Uses the same NSData→NSImage path as the previous PNG-based approach.
fn svg_to_png(svg_bytes: &[u8], muted: bool, theme: Theme) -> Result<(Vec<u8>, u32, u32)> {
    let color = popup_icon_color(muted, theme);
    let (rgba, w, h) = rasterize_svg(svg_bytes, &color)?;
    let img = image::RgbaImage::from_raw(w, h, rgba).context("Failed to create RgbaImage")?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("Failed to encode PNG")?;
    Ok((png, w, h))
}

fn svg_to_ns_image(svg_bytes: &[u8], muted: bool, theme: Theme) -> Result<id> {
    let (png, w, h) = svg_to_png(svg_bytes, muted, theme)?;
    const ICON_HEIGHT: f64 = 16.;
    let icon_width = (w as f64) / (h as f64 / ICON_HEIGHT);
    let ns_image = unsafe {
        let nsdata = NSData::dataWithBytes_length_(
            nil,
            png.as_ptr() as *const std::os::raw::c_void,
            png.len() as u64,
        );
        let ns_image = NSImage::initWithData_(NSImage::alloc(nil), nsdata);
        let _: () = msg_send![ns_image, setSize: NSSize::new(icon_width, ICON_HEIGHT)];
        let _: () = msg_send![ns_image, setTemplate: NO];
        ns_image
    };
    Ok(ns_image)
}

fn get_mic_image(muted: bool, theme: Theme) -> Result<id> {
    const MIC_ON: &[u8] = include_bytes!("../assets/mic.svg");
    const MIC_OFF: &[u8] = include_bytes!("../assets/mic-off.svg");
    svg_to_ns_image(if muted { MIC_OFF } else { MIC_ON }, muted, theme)
}

fn get_camera_image(muted: bool, theme: Theme) -> Result<id> {
    const VIDEO_ON: &[u8] = include_bytes!("../assets/video.svg");
    const VIDEO_OFF: &[u8] = include_bytes!("../assets/video-off.svg");
    svg_to_ns_image(if muted { VIDEO_OFF } else { VIDEO_ON }, muted, theme)
}

fn make_image_view(image: id, frame: NSRect) -> id {
    unsafe {
        let view = NSImageView::alloc(nil);
        let _: () = msg_send![view, initWithFrame: frame];
        view.setImage_(image);
        view
    }
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

#[derive(Copy, Clone)]
pub struct PopupContent {
    mic_label: id,
    mic_image: id,
    camera_image: id,
    camera_label: id,
    pub view: id,
}

impl PopupContent {
    pub fn new(
        mic_muted: bool,
        camera_muted: bool,
        size: LogicalSize<f64>,
        theme: Theme,
    ) -> Result<Self> {
        let frame = get_frame_rect(size);

        let mic_label = get_textfield(
            get_mic_mute_description_text(mic_muted),
            get_text_color(mic_muted, theme),
            frame,
        );
        let mic_ns_image = get_mic_image(mic_muted, theme)?;
        let mic_image = make_image_view(mic_ns_image, frame);
        unsafe {
            let _: () = msg_send![mic_ns_image, release];
        }
        let camera_ns_image = get_camera_image(camera_muted, theme)?;
        let camera_image = make_image_view(camera_ns_image, frame);
        unsafe {
            let _: () = msg_send![camera_ns_image, release];
        }
        let camera_label = get_textfield(
            get_camera_mute_description_text(camera_muted),
            get_text_color(camera_muted, theme),
            frame,
        );

        let view = unsafe {
            let stack: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![stack, initWithFrame: frame];
            const GRAVITY_CENTER: i32 = 2;
            let _: () = msg_send![stack, addView: mic_image inGravity: GRAVITY_CENTER];
            let _: () = msg_send![mic_image, release];
            let _: () = msg_send![stack, addView: mic_label inGravity: GRAVITY_CENTER];
            let _: () = msg_send![mic_label, release];
            let sep = make_separator_view(frame.size.height);
            let _: () = msg_send![stack, addView: sep inGravity: GRAVITY_CENTER];
            let _: () = msg_send![sep, release];
            let _: () = msg_send![stack, addView: camera_image inGravity: GRAVITY_CENTER];
            let _: () = msg_send![camera_image, release];
            let _: () = msg_send![stack, addView: camera_label inGravity: GRAVITY_CENTER];
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

    pub fn update(
        &mut self,
        mic_muted: bool,
        camera_muted: bool,
        theme: Theme,
        _active_device_name: Option<&str>,
    ) -> Result<&mut Self> {
        let mic_img = get_mic_image(mic_muted, theme)?;
        let cam_img = get_camera_image(camera_muted, theme)?;
        unsafe {
            let mic_str = NSString::alloc(nil).init_str(get_mic_mute_description_text(mic_muted));
            self.mic_label.setStringValue_(mic_str);
            let _: () = msg_send![mic_str, release];
            let _: () = msg_send![self.mic_label, setTextColor: get_text_color(mic_muted, theme)];
            self.mic_image.setImage_(mic_img);
            let _: () = msg_send![mic_img, release];
            self.camera_image.setImage_(cam_img);
            let _: () = msg_send![cam_img, release];
            let cam_str =
                NSString::alloc(nil).init_str(get_camera_mute_description_text(camera_muted));
            self.camera_label.setStringValue_(cam_str);
            let _: () = msg_send![cam_str, release];
            let _: () =
                msg_send![self.camera_label, setTextColor: get_text_color(camera_muted, theme)];
        }
        Ok(self)
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
