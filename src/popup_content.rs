use crate::icons::{self, RenderedIcon};
use anyhow::Result;
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSColor, NSFont, NSImage, NSImageView, NSLayoutAttribute, NSLayoutConstraint, NSLayoutRelation,
    NSStackView, NSStackViewGravity, NSTextAlignment, NSTextField, NSView,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use std::sync::Arc;
use tao::dpi::LogicalSize;
use tao::window::Theme;

const MUTED_DESCRIPTION: &str = "Mic off";
const UNMUTED_DESCRIPTION: &str = "Mic on";
const CAMERA_MUTED_DESCRIPTION: &str = "Camera off";
const CAMERA_UNMUTED_DESCRIPTION: &str = "Camera on";

/// The medium popup's measurements, which it has always had; the other sizes scale them
/// (`Metrics::at`). The labels' line height:
const LINE_HEIGHT: f64 = 18.;
/// The icons' height.
const ICON_HEIGHT: f64 = 16.;
/// How much larger than the system font the labels are.
const FONT_SIZE_INCREASE: f64 = 3.;
/// The space between the stack's views: NSStackView's default.
const STACK_SPACING: f64 = 8.;

/// The content's measurements in points at a popup size's scale (`popup::size_scale`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
    line_height: f64,
    icon_height: f64,
    spacing: f64,
    font_size: f64,
}

impl Metrics {
    /// The measurements at `scale`, with labels `FONT_SIZE_INCREASE` above `system_font_size`
    /// before scaling.
    fn at(scale: f64, system_font_size: f64) -> Self {
        Self {
            line_height: LINE_HEIGHT * scale,
            icon_height: ICON_HEIGHT * scale,
            spacing: STACK_SPACING * scale,
            font_size: (system_font_size + FONT_SIZE_INCREASE) * scale,
        }
    }
}

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

/// A vertically centered rect `line_height` tall spanning the full width. Matches the original
/// layout so the NSStackView stays at a fixed size and does not activate Auto Layout resizing
/// on the window.
fn get_frame_rect(size: LogicalSize<f64>, line_height: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(0., (size.height - line_height) / 2.),
        NSSize::new(size.width, line_height),
    )
}

/// A label's red, green, and blue: red while what it names is off, otherwise black on the
/// light theme and white on the dark one.
fn text_rgb(muted: bool, theme: Theme) -> (f64, f64, f64) {
    match theme {
        // 239, 68, 68 (light mode red) - #ef4444
        Theme::Light if muted => (0.9372, 0.2666, 0.2666),
        Theme::Light => (0., 0., 0.),
        // 248, 113, 113 (dark mode red) - #f87171
        Theme::Dark if muted => (0.9725, 0.4431, 0.4431),
        _ => (1., 1., 1.),
    }
}

fn get_text_color(muted: bool, theme: Theme) -> Retained<NSColor> {
    let (red, green, blue) = text_rgb(muted, theme);
    NSColor::colorWithRed_green_blue_alpha(red, green, blue, 1.)
}

/// Sets `label`'s text and color.
fn set_label(label: &NSTextField, text: &str, color: &NSColor) {
    label.setStringValue(&NSString::from_str(text));
    label.setTextColor(Some(color));
}

fn get_textfield(
    mtm: MainThreadMarker,
    text: &str,
    color: &NSColor,
    frame: NSRect,
    font_size: f64,
) -> Retained<NSTextField> {
    let label = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    set_label(&label, text, color);
    label.setBezeled(false);
    label.setEditable(false);
    label.setDrawsBackground(false);
    label.setSelectable(false);
    label.setAlignment(NSTextAlignment::Center);
    label.setFont(Some(&NSFont::systemFontOfSize(font_size)));
    label
}

/// A new `NSImage` of `icon`'s cached PNG, `height` points tall; `None` when AppKit can't read
/// the PNG.
fn ns_image(icon: &RenderedIcon, height: f64) -> Option<Retained<NSImage>> {
    let icon_width = f64::from(icon.width) / (f64::from(icon.height) / height);
    let data = NSData::with_bytes(&icon.png);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setSize(NSSize::new(icon_width, height));
    image.setTemplate(false);
    Some(image)
}

/// Shows `icon`, `height` points tall, in `image_view`. A PNG AppKit can't read leaves the view
/// without an image.
fn set_image(image_view: &NSImageView, icon: &RenderedIcon, height: f64) {
    let image = ns_image(icon, height);
    if image.is_none() {
        log::warn!(
            "AppKit can't read the {}×{} popup icon",
            icon.width,
            icon.height
        );
    }
    image_view.setImage(image.as_deref());
}

fn make_image_view(
    mtm: MainThreadMarker,
    icon: &RenderedIcon,
    frame: NSRect,
    height: f64,
) -> Retained<NSImageView> {
    let view = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
    set_image(&view, icon, height);
    view
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
fn make_separator_view(mtm: MainThreadMarker, line_height: f64) -> Retained<NSView> {
    let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(1., line_height));
    let separator = NSView::initWithFrame(NSView::alloc(mtm), frame);
    separator.setWantsLayer(true);
    if let Some(layer) = separator.layer() {
        let color = NSColor::colorWithWhite_alpha(0.5, 0.5);
        layer.setBackgroundColor(Some(&*color.CGColor()));
    }
    // Size constraints without position constraints — NSStackView controls position.
    separator.setTranslatesAutoresizingMaskIntoConstraints(false);
    for (attribute, constant) in [
        (NSLayoutAttribute::Width, 1.),
        (NSLayoutAttribute::Height, line_height),
    ] {
        // SAFETY: the first item is a view; a constant size relates it to no second item.
        let constraint = unsafe {
            NSLayoutConstraint::constraintWithItem_attribute_relatedBy_toItem_attribute_multiplier_constant(
                &separator,
                attribute,
                NSLayoutRelation::Equal,
                None,
                NSLayoutAttribute::NotAnAttribute,
                1.,
                constant,
            )
        };
        separator.addConstraint(&constraint);
    }
    separator
}

/// What the popup content shows; `Popup` redraws it only when this changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupState {
    pub mic_muted: bool,
    pub camera_in_use: bool,
    pub theme: Theme,
}

/// The views `render` updates. The window's content view holds `view`, the stack view, which
/// holds the others; these references keep them for `render`.
pub struct PopupContent {
    mic_label: Retained<NSTextField>,
    mic_image: Retained<NSImageView>,
    camera_image: Retained<NSImageView>,
    camera_label: Retained<NSTextField>,
    pub view: Retained<NSStackView>,
    /// The icons' height in points, for `render`.
    icon_height: f64,
}

impl PopupContent {
    /// The content of a popup `size` large, drawn at `scale` (`popup::size_scale`).
    pub fn new(
        mtm: MainThreadMarker,
        state: PopupState,
        size: LogicalSize<f64>,
        scale: f64,
    ) -> Result<Self> {
        // Fetch the icons first, so an error creates no view.
        let (mic_icon, camera_icon) = icons_for(state)?;
        // `+[NSFont systemFontSize]`, the size the labels scale from.
        let metrics = Metrics::at(scale, NSFont::systemFontSize());
        let frame = get_frame_rect(size, metrics.line_height);

        let mic_label = get_textfield(
            mtm,
            get_mic_mute_description_text(state.mic_muted),
            &get_text_color(state.mic_muted, state.theme),
            frame,
            metrics.font_size,
        );
        let mic_image = make_image_view(mtm, &mic_icon, frame, metrics.icon_height);
        let camera_image = make_image_view(mtm, &camera_icon, frame, metrics.icon_height);
        let camera_off = !state.camera_in_use;
        let camera_label = get_textfield(
            mtm,
            get_camera_mute_description_text(camera_off),
            &get_text_color(camera_off, state.theme),
            frame,
            metrics.font_size,
        );

        let view = NSStackView::initWithFrame(NSStackView::alloc(mtm), frame);
        view.setSpacing(metrics.spacing);
        let gravity = NSStackViewGravity::Center;
        view.addView_inGravity(&mic_image, gravity);
        view.addView_inGravity(&mic_label, gravity);
        view.addView_inGravity(&make_separator_view(mtm, frame.size.height), gravity);
        view.addView_inGravity(&camera_image, gravity);
        view.addView_inGravity(&camera_label, gravity);

        Ok(Self {
            mic_label,
            mic_image,
            camera_image,
            camera_label,
            view,
            icon_height: metrics.icon_height,
        })
    }

    /// Shows `state` in the labels and icons. On an error nothing has changed yet.
    pub fn render(&self, state: PopupState) -> Result<()> {
        let (mic_icon, camera_icon) = icons_for(state)?;
        let camera_off = !state.camera_in_use;
        set_label(
            &self.mic_label,
            get_mic_mute_description_text(state.mic_muted),
            &get_text_color(state.mic_muted, state.theme),
        );
        set_image(&self.mic_image, &mic_icon, self.icon_height);
        set_image(&self.camera_image, &camera_icon, self.icon_height);
        set_label(
            &self.camera_label,
            get_camera_mute_description_text(camera_off),
            &get_text_color(camera_off, state.theme),
        );
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

    #[test]
    fn medium_metrics_keep_the_original_layout() {
        let medium = Metrics::at(1., 13.);
        assert_eq!(
            medium,
            Metrics {
                line_height: 18.,
                icon_height: 16.,
                spacing: 8.,
                font_size: 16.,
            }
        );
    }

    #[test]
    fn metrics_scale_every_measurement_together() {
        let small = Metrics::at(0.75, 13.);
        assert_eq!((small.font_size, small.icon_height), (12., 12.));
        let large = Metrics::at(1.5, 13.);
        assert_eq!(
            large,
            Metrics {
                line_height: 27.,
                icon_height: 24.,
                spacing: 12.,
                font_size: 24.,
            }
        );
    }

    /// objc2-app-kit takes `NSTextAlignment` from `NSText.h`, which picks its values with
    /// `TARGET_ABI_USES_IOS_VALUES`: the iOS value 1 on Apple silicon, the macOS value 2 on Intel.
    #[test]
    fn labels_center_with_the_sdk_value_for_the_architecture() {
        let center = if cfg!(target_arch = "x86_64") { 2 } else { 1 };
        assert_eq!(NSTextAlignment::Center.0, center);
        // `NSStackView.h`: NSStackViewGravityCenter = 2 on every architecture.
        assert_eq!(NSStackViewGravity::Center.0, 2);
    }

    #[test]
    fn labels_are_red_while_off_and_black_or_white_otherwise() {
        assert_eq!(text_rgb(true, Theme::Light), (0.9372, 0.2666, 0.2666));
        assert_eq!(text_rgb(true, Theme::Dark), (0.9725, 0.4431, 0.4431));
        assert_eq!(text_rgb(false, Theme::Light), (0., 0., 0.));
        assert_eq!(text_rgb(false, Theme::Dark), (1., 1., 1.));
    }

    #[test]
    fn the_line_is_centered_vertically_across_the_full_width() {
        let rect = get_frame_rect(LogicalSize::new(375., 60.), 27.);
        assert_eq!((rect.origin.x, rect.origin.y), (0., 16.5));
        assert_eq!((rect.size.width, rect.size.height), (375., 27.));
    }
}
