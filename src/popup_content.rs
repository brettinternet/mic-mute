use anyhow::{Context, Result};
use cocoa::appkit::{NSColor, NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO};
use cocoa::foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc::runtime::Object;
use tao::dpi::LogicalSize;
use tao::window::Theme;

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

fn get_row_frame(size: LogicalSize<f64>, row: u32) -> NSRect {
    const ROW_HEIGHT: f64 = 22.;
    const PADDING: f64 = 8.;
    // row 0 = top (mic), row 1 = bottom (camera)
    let total_rows = 2;
    let total_height = (ROW_HEIGHT * total_rows as f64) + PADDING;
    let y_start = (size.height - total_height) / 2.;
    let y = y_start + (ROW_HEIGHT * (total_rows - 1 - row) as f64);
    NSRect::new(
        NSPoint::new(0., y),
        NSSize::new(size.width, ROW_HEIGHT),
    )
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

fn make_label(frame: NSRect, text: &str, color: id) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: frame];
        let ns_text = NSString::alloc(nil).init_str(text);
        label.setStringValue_(ns_text);
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        const NSALIGNMENT_CENTER: i32 = 1;
        let _: () = msg_send![label, setAlignment: NSALIGNMENT_CENTER];
        const FONT_INCREASE: f64 = 2.0;
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, systemFontSize];
        let custom_font: *mut Object =
            msg_send![ns_font, systemFontOfSize: default_size + FONT_INCREASE];
        let _: () = msg_send![label, setFont: custom_font];
        label
    }
}

fn make_small_label(frame: NSRect, text: &str, color: id) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: frame];
        let ns_text = NSString::alloc(nil).init_str(text);
        label.setStringValue_(ns_text);
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        const NSALIGNMENT_CENTER: i32 = 1;
        let _: () = msg_send![label, setAlignment: NSALIGNMENT_CENTER];
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, smallSystemFontSize];
        let small_font: *mut Object = msg_send![ns_font, systemFontOfSize: default_size];
        let _: () = msg_send![label, setFont: small_font];
        label
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

        let mic_row_frame = get_row_frame(size, 0);
        let camera_row_frame = get_row_frame(size, 1);
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

        let mic_label = make_label(mic_row_frame, get_mic_mute_description_text(mic_muted), initial_mic_color);
        let mic_device_label = make_small_label(mic_row_frame, "", initial_device_color);
        let camera_label = make_label(camera_row_frame, get_camera_mute_description_text(camera_muted), initial_camera_color);

        let mic_image_view = unsafe {
            let image_view = NSImageView::alloc(nil);
            let _: () = msg_send![image_view, initWithFrame: mic_row_frame];
            image_view.setImage_(initial_mic_image);
            image_view
        };

        let view = unsafe {
            let stack_view: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![stack_view, initWithFrame: view_frame];
            const NS_USER_INTERFACE_LAYOUT_ORIENTATION_VERTICAL: i64 = 1;
            let _: () = msg_send![stack_view, setOrientation: NS_USER_INTERFACE_LAYOUT_ORIENTATION_VERTICAL];
            const NS_STACK_VIEW_GRAVITY_CENTER: i32 = 2;

            // Mic row: icon + status label + (optional) device name label
            let mic_row: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![mic_row, initWithFrame: mic_row_frame];
            let _: () = msg_send![mic_row, addView: mic_image_view inGravity: NS_STACK_VIEW_GRAVITY_CENTER];
            let _: () = msg_send![mic_row, addView: mic_label inGravity: NS_STACK_VIEW_GRAVITY_CENTER];
            let _: () = msg_send![mic_row, addView: mic_device_label inGravity: NS_STACK_VIEW_GRAVITY_CENTER];

            // Camera row: status label only
            let camera_row: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![camera_row, initWithFrame: camera_row_frame];
            let _: () = msg_send![camera_row, addView: camera_label inGravity: NS_STACK_VIEW_GRAVITY_CENTER];

            let _: () = msg_send![stack_view, addView: mic_row inGravity: NS_STACK_VIEW_GRAVITY_CENTER];
            let _: () = msg_send![stack_view, addView: camera_row inGravity: NS_STACK_VIEW_GRAVITY_CENTER];

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
