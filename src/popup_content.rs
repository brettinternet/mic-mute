use anyhow::{Context, Result};
use cocoa::appkit::{NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO};
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

const MUTED_DESCRIPTION: &str = "Mic off";
const UNMUTED_DESCRIPTION: &str = "Mic on";
const CAMERA_MUTED_DESCRIPTION: &str = "Camera off";
const CAMERA_UNMUTED_DESCRIPTION: &str = "Camera on";

pub fn get_mic_mute_description_text(muted: bool) -> &'static str {
    if muted { MUTED_DESCRIPTION } else { UNMUTED_DESCRIPTION }
}

pub fn get_camera_mute_description_text(muted: bool) -> &'static str {
    if muted { CAMERA_MUTED_DESCRIPTION } else { CAMERA_UNMUTED_DESCRIPTION }
}

fn zero_rect() -> NSRect {
    NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.))
}

fn get_frame_rect(size: LogicalSize<f64>) -> NSRect {
    NSRect::new(NSPoint::new(0., 0.), NSSize::new(size.width, size.height))
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

/// System-adaptive primary text color. Black in light mode, white in dark mode.
/// Resolves against the view's effective appearance at draw time.
fn label_color() -> id {
    unsafe { msg_send![class!(NSColor), labelColor] }
}

/// System-adaptive red for muted state. Renders correctly in both modes.
fn muted_color() -> id {
    unsafe { msg_send![class!(NSColor), systemRedColor] }
}

fn text_color(muted: bool) -> id {
    if muted { muted_color() } else { label_color() }
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
        let size: f64 = msg_send![ns_font, systemFontSize];
        let font: *mut Object = msg_send![ns_font, systemFontOfSize: size];
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

#[allow(dead_code)]
pub struct PopupContent {
    mic_label: id,
    mic_image_view: id,
    camera_label: id,
    pub view: id,
    // Cached images (created once, reused on every update)
    image_muted_light: id,
    image_muted_dark: id,
    image_unmuted_light: id,
    image_unmuted_dark: id,
    // Cached label text (no NSString allocation per update)
    ns_text_mic_on: id,
    ns_text_mic_off: id,
    ns_text_camera_on: id,
    ns_text_camera_off: id,
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

        let image_muted_light = make_ns_image(DARK_MIC_OFF, ICON_HEIGHT)?;
        let image_muted_dark = make_ns_image(LIGHT_MIC_OFF, ICON_HEIGHT)?;
        let image_unmuted_light = make_ns_image(DARK_MIC_ON, ICON_HEIGHT)?;
        let image_unmuted_dark = make_ns_image(LIGHT_MIC_ON, ICON_HEIGHT)?;

        let ns_text_mic_on = unsafe { NSString::alloc(nil).init_str(UNMUTED_DESCRIPTION) };
        let ns_text_mic_off = unsafe { NSString::alloc(nil).init_str(MUTED_DESCRIPTION) };
        let ns_text_camera_on = unsafe { NSString::alloc(nil).init_str(CAMERA_UNMUTED_DESCRIPTION) };
        let ns_text_camera_off = unsafe { NSString::alloc(nil).init_str(CAMERA_MUTED_DESCRIPTION) };

        let initial_mic_image = Self::pick_mic_image(
            mic_muted, theme,
            image_muted_light, image_muted_dark, image_unmuted_light, image_unmuted_dark,
        );

        let mic_label = make_label(get_mic_mute_description_text(mic_muted), text_color(mic_muted));
        let camera_label = make_label(get_camera_mute_description_text(camera_muted), text_color(camera_muted));
        let mic_image_view = make_image_view(initial_mic_image);

        let view_frame = get_frame_rect(size);

        let view = unsafe {
            const HORIZONTAL: i64 = 0;
            const ALIGN_CENTER_Y: i32 = 9;

            let stack: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![stack, initWithFrame: view_frame];
            let _: () = msg_send![stack, setOrientation: HORIZONTAL];
            let _: () = msg_send![stack, setAlignment: ALIGN_CENTER_Y];
            let _: () = msg_send![stack, setSpacing: 8.0_f64];
            let insets = NSEdgeInsets { top: 10., left: 14., bottom: 10., right: 14. };
            let _: () = msg_send![stack, setEdgeInsets: insets];

            let _: () = msg_send![stack, addArrangedSubview: mic_image_view];
            let _: () = msg_send![stack, addArrangedSubview: mic_label];
            let _: () = msg_send![stack, setCustomSpacing: 18.0_f64 afterView: mic_label];
            let _: () = msg_send![stack, addArrangedSubview: camera_label];

            stack
        };

        Ok(Self {
            mic_label,
            mic_image_view,
            camera_label,
            view,
            image_muted_light,
            image_muted_dark,
            image_unmuted_light,
            image_unmuted_dark,
            ns_text_mic_on,
            ns_text_mic_off,
            ns_text_camera_on,
            ns_text_camera_off,
        })
    }

    fn pick_mic_image(muted: bool, theme: Theme, ml: id, md: id, ul: id, ud: id) -> id {
        match theme {
            Theme::Light if muted => ml,
            Theme::Light => ul,
            Theme::Dark if muted => md,
            _ => ud,
        }
    }

    pub fn update(
        &mut self,
        mic_muted: bool,
        camera_muted: bool,
        theme: Theme,
        _active_device_name: Option<&str>,
    ) -> Result<&mut Self> {
        let img = Self::pick_mic_image(
            mic_muted, theme,
            self.image_muted_light, self.image_muted_dark,
            self.image_unmuted_light, self.image_unmuted_dark,
        );
        let ns_mic_text = if mic_muted { self.ns_text_mic_off } else { self.ns_text_mic_on };
        let ns_camera_text = if camera_muted { self.ns_text_camera_off } else { self.ns_text_camera_on };

        unsafe {
            self.mic_image_view.setImage_(img);
            self.mic_label.setStringValue_(ns_mic_text);
            let _: () = msg_send![self.mic_label, setTextColor: text_color(mic_muted)];
            self.camera_label.setStringValue_(ns_camera_text);
            let _: () = msg_send![self.camera_label, setTextColor: text_color(camera_muted)];
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

    #[test]
    fn test_camera_mute_description_muted() {
        assert_eq!(get_camera_mute_description_text(true), "Camera off");
    }

    #[test]
    fn test_camera_mute_description_unmuted() {
        assert_eq!(get_camera_mute_description_text(false), "Camera on");
    }
}
