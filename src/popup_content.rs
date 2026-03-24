use anyhow::{Context, Result};
use cocoa::appkit::{NSColor, NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO, YES};
use cocoa::foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc::runtime::Object;
use tao::dpi::LogicalSize;
use tao::window::Theme;

/// NSEdgeInsets for passing to `setEdgeInsets:` on NSStackView.
#[repr(C)]
#[derive(Clone, Copy)]
struct NSEdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

const MUTED_DESCRIPTION: &str = "Microphone off";
const UNMUTED_DESCRIPTION: &str = "Microphone on";
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

fn zero_rect() -> NSRect {
    NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.))
}

fn get_frame_rect(size: LogicalSize<f64>) -> NSRect {
    NSRect::new(
        NSPoint::new(0., 0.),
        NSSize::new(size.width, size.height),
    )
}

fn make_ns_color(r: f64, g: f64, b: f64, a: f64) -> id {
    unsafe { NSColor::colorWithRed_green_blue_alpha_(nil, r, g, b, a) }
}

fn make_ns_image(bytes: &[u8], icon_height: f64) -> Result<id> {
    let image_buff = image::load_from_memory(bytes)
        .context("Failed to open icon image path")?
        .into_rgba8();
    let (width, height) = image_buff.dimensions();
    let icon_width: f64 = (width as f64) / (height as f64 / icon_height);

    let ns_image = unsafe {
        let nsdata = NSData::dataWithBytes_length_(
            nil,
            bytes.as_ptr() as *const std::os::raw::c_void,
            bytes.len() as u64,
        );
        let ns_image = NSImage::initWithData_(NSImage::alloc(nil), nsdata);
        let size = NSSize::new(icon_width, icon_height);
        let _: () = msg_send![ns_image, setSize: size];
        let _: () = msg_send![ns_image, setTemplate: NO];
        ns_image
    };
    Ok(ns_image)
}

fn make_label(text: &str, color: id) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: zero_rect()];
        let ns_text = NSString::alloc(nil).init_str(text);
        label.setStringValue_(ns_text);
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, systemFontSize];
        let font: *mut Object = msg_send![ns_font, systemFontOfSize: default_size];
        let _: () = msg_send![label, setFont: font];
        label
    }
}

fn make_small_label(text: &str, color: id) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: zero_rect()];
        let ns_text = NSString::alloc(nil).init_str(text);
        label.setStringValue_(ns_text);
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let ns_font = class!(NSFont);
        let small_size: f64 = msg_send![ns_font, smallSystemFontSize];
        let font: *mut Object = msg_send![ns_font, systemFontOfSize: small_size];
        let _: () = msg_send![label, setFont: font];
        label
    }
}

fn make_image_view(image: id) -> id {
    unsafe {
        let image_view = NSImageView::alloc(nil);
        let _: () = msg_send![image_view, initWithFrame: zero_rect()];
        image_view.setImage_(image);
        image_view
    }
}

/// Build a horizontal NSStackView row: [icon  label  subtitle?]
fn make_row(icon: id, label: id, subtitle: Option<id>) -> id {
    unsafe {
        const HORIZONTAL: i64 = 0;
        const GRAVITY_CENTER: i32 = 2;
        const ALIGN_CENTER_Y: i32 = 9; // NSLayoutAttributeCenterY

        let row: *mut Object = msg_send![class!(NSStackView), alloc];
        let _: () = msg_send![row, initWithFrame: zero_rect()];
        let _: () = msg_send![row, setOrientation: HORIZONTAL];
        let _: () = msg_send![row, setAlignment: ALIGN_CENTER_Y];
        let _: () = msg_send![row, setSpacing: 6.0_f64];
        let _: () = msg_send![row, addView: icon inGravity: GRAVITY_CENTER];
        let _: () = msg_send![row, addView: label inGravity: GRAVITY_CENTER];
        if let Some(sub) = subtitle {
            let _: () = msg_send![row, addView: sub inGravity: GRAVITY_CENTER];
        }
        row
    }
}

fn make_separator() -> id {
    unsafe {
        // NSBox with NSBoxSeparator (type 2) draws a native 1pt horizontal rule
        const NS_BOX_SEPARATOR: i32 = 2;
        let sep: *mut Object = msg_send![class!(NSBox), alloc];
        let _: () = msg_send![sep, init];
        let _: () = msg_send![sep, setBoxType: NS_BOX_SEPARATOR];
        sep
    }
}

fn make_cached_ns_string(text: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(text) }
}

#[allow(dead_code)]
pub struct PopupContent {
    mic_label: id,
    mic_device_label: id,
    mic_image_view: id,
    camera_label: id,
    pub view: id,
    // Cached images (created once, reused on update)
    image_muted_light: id,
    image_muted_dark: id,
    image_unmuted_light: id,
    image_unmuted_dark: id,
    // Cached colors (created once, reused on update)
    color_muted_light: id,
    color_muted_dark: id,
    color_unmuted_light: id,
    color_unmuted_dark: id,
    // Camera cached colors (reuse same colors)
    color_camera_muted_light: id,
    color_camera_muted_dark: id,
    color_camera_unmuted_light: id,
    color_camera_unmuted_dark: id,
    // Cached NSString objects for label text (no allocations in update)
    ns_text_mic_on: id,
    ns_text_mic_off: id,
    ns_text_camera_on: id,
    ns_text_camera_off: id,
    // Cached gray color for device name subtitle
    color_gray_light: id,
    color_gray_dark: id,
}

unsafe impl Send for PopupContent {}
unsafe impl Sync for PopupContent {}

impl PopupContent {
    pub fn new(mic_muted: bool, camera_muted: bool, size: LogicalSize<f64>, theme: Theme) -> Result<Self> {
        const DARK_MIC_ON: &[u8] = include_bytes!("../assets/images/mic.png");
        const DARK_MIC_OFF: &[u8] = include_bytes!("../assets/images/mic-off.png");
        const LIGHT_MIC_ON: &[u8] = include_bytes!("../assets/images/mic-light.png");
        const LIGHT_MIC_OFF: &[u8] = include_bytes!("../assets/images/mic-off-light.png");
        const ICON_HEIGHT: f64 = 16.0;

        // Create all cached images once
        let image_muted_light = make_ns_image(DARK_MIC_OFF, ICON_HEIGHT)?;
        let image_muted_dark = make_ns_image(LIGHT_MIC_OFF, ICON_HEIGHT)?;
        let image_unmuted_light = make_ns_image(DARK_MIC_ON, ICON_HEIGHT)?;
        let image_unmuted_dark = make_ns_image(LIGHT_MIC_ON, ICON_HEIGHT)?;

        // Mic colors: dark_red for light theme, light_red for dark theme
        let color_muted_light = make_ns_color(0.9372, 0.2666, 0.2666, 1.);
        let color_muted_dark = make_ns_color(0.9725, 0.4431, 0.4431, 1.);
        let color_unmuted_light = make_ns_color(0., 0., 0., 1.);
        let color_unmuted_dark = make_ns_color(1., 1., 1., 1.);

        // Camera colors (same palette)
        let color_camera_muted_light = make_ns_color(0.9372, 0.2666, 0.2666, 1.);
        let color_camera_muted_dark = make_ns_color(0.9725, 0.4431, 0.4431, 1.);
        let color_camera_unmuted_light = make_ns_color(0., 0., 0., 1.);
        let color_camera_unmuted_dark = make_ns_color(1., 1., 1., 1.);

        // Cached label text strings (no per-update allocs)
        let ns_text_mic_on = make_cached_ns_string(UNMUTED_DESCRIPTION);
        let ns_text_mic_off = make_cached_ns_string(MUTED_DESCRIPTION);
        let ns_text_camera_on = make_cached_ns_string(CAMERA_UNMUTED_DESCRIPTION);
        let ns_text_camera_off = make_cached_ns_string(CAMERA_MUTED_DESCRIPTION);

        // Subtitle colors for device name
        let color_gray_light = make_ns_color(0.4, 0.4, 0.4, 1.);
        let color_gray_dark = make_ns_color(0.6, 0.6, 0.6, 1.);

        let view_frame = get_frame_rect(size);

        let (initial_mic_image, initial_mic_color) = Self::pick_mic_image_and_color(
            mic_muted, theme,
            image_muted_light, image_muted_dark, image_unmuted_light, image_unmuted_dark,
            color_muted_light, color_muted_dark, color_unmuted_light, color_unmuted_dark,
        );

        let initial_camera_color = Self::pick_camera_color(
            camera_muted, theme,
            color_camera_muted_light, color_camera_muted_dark,
            color_camera_unmuted_light, color_camera_unmuted_dark,
        );

        let initial_device_color = if theme == Theme::Dark { color_gray_dark } else { color_gray_light };

        let mic_label = make_label(get_mic_mute_description_text(mic_muted), initial_mic_color);
        let mic_device_label = make_small_label("", initial_device_color);
        let camera_label = make_label(get_camera_mute_description_text(camera_muted), initial_camera_color);

        let mic_image_view = make_image_view(initial_mic_image);
        let mic_row = make_row(mic_image_view, mic_label, Some(mic_device_label));
        let camera_row = make_row(make_image_view(nil), camera_label, None);
        let separator = make_separator();

        let view = unsafe {
            const VERTICAL: i64 = 1;
            const GRAVITY_CENTER: i32 = 2;
            const ALIGN_CENTER_X: i32 = 10; // NSLayoutAttributeCenterX

            let stack_view: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![stack_view, initWithFrame: view_frame];
            let _: () = msg_send![stack_view, setOrientation: VERTICAL];
            let _: () = msg_send![stack_view, setAlignment: ALIGN_CENTER_X];
            let _: () = msg_send![stack_view, setSpacing: 6.0_f64];
            let insets = NSEdgeInsets { top: 12., left: 14., bottom: 12., right: 14. };
            let _: () = msg_send![stack_view, setEdgeInsets: insets];

            let _: () = msg_send![stack_view, addView: mic_row inGravity: GRAVITY_CENTER];
            let _: () = msg_send![stack_view, addView: separator inGravity: GRAVITY_CENTER];
            let _: () = msg_send![stack_view, addView: camera_row inGravity: GRAVITY_CENTER];

            // Make the separator stretch full width
            let _: () = msg_send![separator, setTranslatesAutoresizingMaskIntoConstraints: YES];

            stack_view
        };

        Ok(Self {
            mic_label,
            mic_device_label,
            mic_image_view,
            camera_label,
            view,
            image_muted_light,
            image_muted_dark,
            image_unmuted_light,
            image_unmuted_dark,
            color_muted_light,
            color_muted_dark,
            color_unmuted_light,
            color_unmuted_dark,
            color_camera_muted_light,
            color_camera_muted_dark,
            color_camera_unmuted_light,
            color_camera_unmuted_dark,
            ns_text_mic_on,
            ns_text_mic_off,
            ns_text_camera_on,
            ns_text_camera_off,
            color_gray_light,
            color_gray_dark,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn pick_mic_image_and_color(
        muted: bool,
        theme: Theme,
        image_muted_light: id,
        image_muted_dark: id,
        image_unmuted_light: id,
        image_unmuted_dark: id,
        color_muted_light: id,
        color_muted_dark: id,
        color_unmuted_light: id,
        color_unmuted_dark: id,
    ) -> (id, id) {
        match theme {
            Theme::Light if muted => (image_muted_light, color_muted_light),
            Theme::Light => (image_unmuted_light, color_unmuted_light),
            Theme::Dark if muted => (image_muted_dark, color_muted_dark),
            _ => (image_unmuted_dark, color_unmuted_dark),
        }
    }

    fn pick_camera_color(
        muted: bool,
        theme: Theme,
        color_muted_light: id,
        color_muted_dark: id,
        color_unmuted_light: id,
        color_unmuted_dark: id,
    ) -> id {
        match theme {
            Theme::Light if muted => color_muted_light,
            Theme::Light => color_unmuted_light,
            Theme::Dark if muted => color_muted_dark,
            _ => color_unmuted_dark,
        }
    }

    pub fn update(
        &mut self,
        mic_muted: bool,
        camera_muted: bool,
        theme: Theme,
        active_device_name: Option<&str>,
    ) -> Result<&mut Self> {
        let (img, mic_color) = Self::pick_mic_image_and_color(
            mic_muted,
            theme,
            self.image_muted_light,
            self.image_muted_dark,
            self.image_unmuted_light,
            self.image_unmuted_dark,
            self.color_muted_light,
            self.color_muted_dark,
            self.color_unmuted_light,
            self.color_unmuted_dark,
        );

        let camera_color = Self::pick_camera_color(
            camera_muted,
            theme,
            self.color_camera_muted_light,
            self.color_camera_muted_dark,
            self.color_camera_unmuted_light,
            self.color_camera_unmuted_dark,
        );

        // Use cached NSString objects — zero new Cocoa allocations per update
        let ns_mic_text = if mic_muted { self.ns_text_mic_off } else { self.ns_text_mic_on };
        let ns_camera_text = if camera_muted { self.ns_text_camera_off } else { self.ns_text_camera_on };
        let device_color = if theme == Theme::Dark { self.color_gray_dark } else { self.color_gray_light };

        unsafe {
            self.mic_label.setStringValue_(ns_mic_text);
            let _: () = msg_send![self.mic_label, setTextColor: mic_color];
            self.mic_image_view.setImage_(img);

            self.camera_label.setStringValue_(ns_camera_text);
            let _: () = msg_send![self.camera_label, setTextColor: camera_color];

            // Update active device name subtitle (allocates one small NSString only if name changed)
            if let Some(name) = active_device_name {
                let truncated = if name.len() > 20 { &name[..20] } else { name };
                let ns_device = NSString::alloc(nil).init_str(truncated);
                self.mic_device_label.setStringValue_(ns_device);
                let _: () = msg_send![self.mic_device_label, setTextColor: device_color];
            }
        };

        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mic_mute_description_muted() {
        assert_eq!(get_mic_mute_description_text(true), "Microphone off");
    }

    #[test]
    fn test_mic_mute_description_unmuted() {
        assert_eq!(get_mic_mute_description_text(false), "Microphone on");
    }

    #[test]
    fn test_camera_mute_description_muted() {
        assert_eq!(get_camera_mute_description_text(true), "Camera off");
    }

    #[test]
    fn test_camera_mute_description_unmuted() {
        assert_eq!(get_camera_mute_description_text(false), "Camera on");
    }
}
