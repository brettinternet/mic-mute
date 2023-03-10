use anyhow::{Context, Result};
use cocoa::appkit::{NSColor, NSImage, NSImageView, NSTextField};
use cocoa::base::{id, nil, NO};
use cocoa::foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use objc::runtime::Object;
use tao::dpi::LogicalSize;

const MUTED_DESCRIPTION: &str = "Microphone off";
const UNMUTED_DESCRIPTION: &str = "Microphone on";
pub fn get_mic_mute_description_text(muted: bool) -> &'static str {
    if muted {
        MUTED_DESCRIPTION
    } else {
        UNMUTED_DESCRIPTION
    }
}

fn get_frame_rect(size: LogicalSize<f64>) -> NSRect {
    const LINE_HEIGHT: f64 = 18.;
    NSRect::new(
        NSPoint::new(0., (size.height - LINE_HEIGHT) / 2.),
        NSSize::new(size.width, LINE_HEIGHT),
    )
}

fn get_mic_mute_color(muted: bool) -> id {
    let (black, white, dark_red, light_red) = unsafe {
        // 239, 68, 68
        let dark_red = NSColor::colorWithRed_green_blue_alpha_(nil, 0.9372, 0.2666, 0.2666, 1.);
        // 248, 113, 113
        let light_red = NSColor::colorWithRed_green_blue_alpha_(nil, 0.9725, 0.4431, 0.4431, 1.);
        let black = NSColor::colorWithRed_green_blue_alpha_(nil, 0., 0., 0., 1.);
        let white = NSColor::colorWithRed_green_blue_alpha_(nil, 1., 1., 1., 1.);
        (black, white, dark_red, light_red)
    };

    match dark_light::detect() {
        dark_light::Mode::Light if muted => dark_red,
        dark_light::Mode::Light if !muted => black,
        dark_light::Mode::Dark if muted => light_red,
        _ => white,
    }
}

fn get_textfield(muted: bool, frame: NSRect) -> id {
    unsafe {
        let label = NSTextField::alloc(nil);
        let _: () = msg_send![label, initWithFrame: frame];
        let text = get_mic_mute_description_text(muted);
        label.setStringValue_(NSString::alloc(nil).init_str(text));
        let color = get_mic_mute_color(muted);
        let _: () = msg_send![label, setTextColor: color];

        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setSelectable: NO];
        const NSALIGNMENT_CENTER: i32 = 1;
        let _: () = msg_send![label, setAlignment: NSALIGNMENT_CENTER];

        const FONT_INCREASE: f64 = 3.0;
        let ns_font = class!(NSFont);
        let default_size: f64 = msg_send![ns_font, systemFontSize];
        let custom_font: *mut Object =
            msg_send![ns_font, systemFontOfSize: default_size + FONT_INCREASE];
        let _: () = msg_send![label, setFont: custom_font];

        label
    }
}

fn get_image(muted: bool) -> Result<id> {
    const DARK_MIC_ON: &[u8] = include_bytes!("../assets/images/mic.png");
    const DARK_MIC_OFF: &[u8] = include_bytes!("../assets/images/mic-off.png");
    const LIGHT_MIC_ON: &[u8] = include_bytes!("../assets/images/mic-light.png");
    const LIGHT_MIC_OFF: &[u8] = include_bytes!("../assets/images/mic-off-light.png");

    let image = match dark_light::detect() {
        dark_light::Mode::Light if muted => DARK_MIC_OFF,
        dark_light::Mode::Light if !muted => DARK_MIC_ON,
        dark_light::Mode::Dark if muted => LIGHT_MIC_OFF,
        _ => LIGHT_MIC_ON,
    };

    let image_buff = image::load_from_memory(image)
        .context("Failed to open icon image path")?
        .into_rgba8();
    let (width, height) = image_buff.dimensions();

    let icon_height: f64 = 16.0;
    let icon_width: f64 = (width as f64) / (height as f64 / icon_height);

    let ns_image = unsafe {
        let nsdata = NSData::dataWithBytes_length_(
            nil,
            image.as_ptr() as *const std::os::raw::c_void,
            image.len() as u64,
        );

        let ns_image = NSImage::initWithData_(NSImage::alloc(nil), nsdata);
        let size = NSSize::new(icon_width, icon_height);

        let _: () = msg_send![ns_image, setSize: size];
        let _: () = msg_send![ns_image, setTemplate: NO];

        ns_image
    };

    Ok(ns_image)
}

#[derive(Copy, Clone)]
pub struct PopupContent {
    label: id,
    image: id,
    pub view: id,
}

/// TODO: set image
/// https://github.com/tauri-apps/tray-icon/blob/b4fc8f888a07cb66661cf15d0da9d39951995e04/src/platform_impl/macos/mod.rs#L155
impl PopupContent {
    pub fn new(muted: bool, size: LogicalSize<f64>) -> Result<Self> {
        let frame = get_frame_rect(size);
        let label = get_textfield(muted, frame);
        let image = get_image(muted)?;
        let image = unsafe {
            let image_view = NSImageView::alloc(nil);
            let _: () = msg_send![image_view, initWithFrame: frame];
            image_view.setImage_(image);
            image_view
        };

        let view = unsafe {
            // NSStackView e.g. https://github.com/balthild/native-dialog-rs/blob/d2ddd443f8c01e92dc22cc8132159c1b9598eaca/src/dialog_impl/mac/file.rs#L130
            // https://developer.apple.com/documentation/appkit/nsstackview?language=objc
            let stack_view: *mut Object = msg_send![class!(NSStackView), alloc];
            let _: () = msg_send![stack_view, initWithFrame: frame];
            const NS_STACK_VIEW_GRAVITY_CENTER: i32 = 2;
            let _: () =
                msg_send![stack_view, addView: image inGravity: NS_STACK_VIEW_GRAVITY_CENTER];
            let _: () =
                msg_send![stack_view, addView: label inGravity: NS_STACK_VIEW_GRAVITY_CENTER];

            stack_view
        };

        let content = Self { label, image, view };
        Ok(content)
    }

    pub fn update(&mut self, muted: bool) -> Result<&mut Self> {
        let text = get_mic_mute_description_text(muted);
        let color = get_mic_mute_color(muted);
        let img = get_image(muted)?;

        unsafe {
            self.label
                .setStringValue_(NSString::alloc(nil).init_str(text));
            let _: () = msg_send![self.label, setTextColor: color];
            self.image.setImage_(img);
        };

        Ok(self)
    }
}
