use anyhow::{Context, Result};
use image::ImageEncoder;
use parking_lot::Mutex;
use std::sync::Arc;
use tao::window::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IconColor {
    r: u8,
    g: u8,
    b: u8,
}

/// #ef4444: muted, or no camera in use, on a light background; the muted tray icon.
const RED: IconColor = IconColor {
    r: 239,
    g: 68,
    b: 68,
};
/// #f87171: muted, or no camera in use, on a dark background.
const LIGHT_RED: IconColor = IconColor {
    r: 248,
    g: 113,
    b: 113,
};
const BLACK: IconColor = IconColor { r: 0, g: 0, b: 0 };
const WHITE: IconColor = IconColor {
    r: 255,
    g: 255,
    b: 255,
};

fn popup_icon_color(muted: bool, theme: Theme) -> IconColor {
    match theme {
        Theme::Light if muted => RED,
        Theme::Light => BLACK,
        Theme::Dark if muted => LIGHT_RED,
        _ => WHITE,
    }
}

/// An SVG embedded from `assets/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyph {
    Mic,
    MicOff,
    Video,
    VideoOff,
}

impl Glyph {
    fn mic(muted: bool) -> Self {
        if muted {
            Self::MicOff
        } else {
            Self::Mic
        }
    }

    fn svg(self) -> &'static [u8] {
        match self {
            Self::Mic => include_bytes!("../assets/mic.svg"),
            Self::MicOff => include_bytes!("../assets/mic-off.svg"),
            Self::Video => include_bytes!("../assets/video.svg"),
            Self::VideoOff => include_bytes!("../assets/video-off.svg"),
        }
    }
}

/// One icon in one color. Only `tray_mic`, `popup_mic`, and `popup_camera` build variants: 8
/// distinct ones, as the tray's icons are the light-theme popup's mic icons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IconVariant {
    glyph: Glyph,
    color: IconColor,
}

/// An icon variant as the tray shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayImage {
    pub variant: IconVariant,
    /// AppKit draws a template image from its alpha alone, in the menu bar's text color.
    pub is_template: bool,
}

/// The tray's mic icon. Unmuted, it's a black template image, which macOS tints for light and
/// dark menu bars and for the highlight while the menu is open. Muted, it's red and no
/// template: red reads on both.
pub fn tray_mic(muted: bool) -> TrayImage {
    TrayImage {
        variant: IconVariant {
            glyph: Glyph::mic(muted),
            color: if muted { RED } else { BLACK },
        },
        is_template: !muted,
    }
}

/// The popup's mic icon: red while muted, else the theme's text color.
pub fn popup_mic(muted: bool, theme: Theme) -> IconVariant {
    IconVariant {
        glyph: Glyph::mic(muted),
        color: popup_icon_color(muted, theme),
    }
}

/// The popup's camera icon: the theme's text color while a camera is in use, else red, like a
/// muted mic.
pub fn popup_camera(in_use: bool, theme: Theme) -> IconVariant {
    IconVariant {
        glyph: if in_use {
            Glyph::Video
        } else {
            Glyph::VideoOff
        },
        color: popup_icon_color(!in_use, theme),
    }
}

/// An icon variant's pixels, rendered once and kept for the process lifetime.
pub struct RenderedIcon {
    /// Un-premultiplied RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The same pixels PNG-encoded.
    pub png: Vec<u8>,
}

/// Every variant rendered so far. Holds plain bytes, never AppKit objects, so nothing in it
/// needs a retain or a release; callers build `NSImage`s and tray icons from it when the shown
/// variant changes. Bounded by the variant count (8 entries of a few KB each), so it never
/// evicts.
static RENDERED: Mutex<Vec<(IconVariant, Arc<RenderedIcon>)>> = Mutex::new(Vec::new());

impl IconVariant {
    /// This variant rasterized and PNG-encoded: rendered on first use, cached after that.
    pub fn rendered(self) -> Result<Arc<RenderedIcon>> {
        let mut rendered = RENDERED.lock();
        if let Some((_, icon)) = rendered.iter().find(|(variant, _)| *variant == self) {
            return Ok(Arc::clone(icon));
        }
        let icon = Arc::new(self.render()?);
        rendered.push((self, Arc::clone(&icon)));
        Ok(icon)
    }

    fn render(self) -> Result<RenderedIcon> {
        let (rgba, width, height) = rasterize_svg(self.glyph.svg(), &self.color)?;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .context("Failed to encode PNG")?;
        Ok(RenderedIcon {
            rgba,
            width,
            height,
            png,
        })
    }
}

/// Rasterizes an SVG with the given stroke color.
/// Returns un-premultiplied RGBA bytes plus the source dimensions.
fn rasterize_svg(svg_bytes: &[u8], color: &IconColor) -> Result<(Vec<u8>, u32, u32)> {
    let svg_str = std::str::from_utf8(svg_bytes).context("SVG is not valid UTF-8")?;
    let colored = svg_str.replacen(
        "<svg ",
        &format!("<svg stroke=\"rgb({},{},{})\" ", color.r, color.g, color.b),
        1,
    );

    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(&colored, &options).context("Failed to parse SVG")?;
    let size = tree.size();
    let w = size.width() as u32;
    let h = size.height() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("Failed to allocate pixmap")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );

    // tiny-skia produces premultiplied RGBA; un-premultiply for callers.
    let raw = pixmap.take();
    let (chunks, _) = raw.as_chunks::<4>();
    let straight: Vec<u8> = chunks
        .iter()
        .flat_map(|p: &[u8; 4]| {
            let a = p[3];
            if a == 0 {
                [0u8, 0, 0, 0]
            } else {
                let s = 255.0_f32 / a as f32;
                [
                    (p[0] as f32 * s).min(255.) as u8,
                    (p[1] as f32 * s).min(255.) as u8,
                    (p[2] as f32 * s).min(255.) as u8,
                    a,
                ]
            }
        })
        .collect();

    Ok((straight, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Icons the app embeds with `include_bytes!` (`Glyph::svg`).
    const EMBEDDED_ICONS: [&str; 4] = ["mic.svg", "mic-off.svg", "video.svg", "video-off.svg"];

    fn asset_svg_paths() -> Vec<PathBuf> {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&assets)
            .expect("assets/ should be readable")
            .map(|entry| entry.expect("assets/ entry should be readable").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
            .collect();
        paths.sort();
        paths
    }

    /// Every variant the tray and the popup can show.
    fn every_variant() -> Vec<IconVariant> {
        let mut variants = Vec::new();
        for state in [true, false] {
            variants.push(tray_mic(state).variant);
            for theme in [Theme::Light, Theme::Dark] {
                variants.push(popup_mic(state, theme));
                variants.push(popup_camera(state, theme));
            }
        }
        variants
    }

    #[test]
    fn every_asset_svg_rasterizes_to_visible_pixels() {
        let paths = asset_svg_paths();
        for icon in EMBEDDED_ICONS {
            assert!(
                paths.iter().any(|path| path.ends_with(icon)),
                "assets/{icon} not found in {paths:?}"
            );
        }

        for path in &paths {
            let name = path.display();
            let svg = std::fs::read(path).expect("SVG should be readable");
            let (rgba, w, h) = rasterize_svg(&svg, &RED)
                .unwrap_or_else(|e| panic!("{name} failed to rasterize: {e:#}"));
            assert!(w > 0 && h > 0, "{name} rasterized to {w}x{h}");
            assert_eq!(rgba.len(), w as usize * h as usize * 4, "{name}");
            assert!(
                rgba.chunks_exact(4).any(|pixel| pixel[3] > 0),
                "{name} rendered no visible pixels"
            );
        }
    }

    #[test]
    fn rasterize_svg_strokes_opaque_pixels_in_the_requested_color() {
        let color = popup_icon_color(true, Theme::Light);
        let (rgba, _, _) =
            rasterize_svg(include_bytes!("../assets/mic.svg"), &color).expect("mic.svg rasterizes");
        let opaque: Vec<&[u8]> = rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[3] == u8::MAX)
            .collect();
        assert!(!opaque.is_empty(), "mic.svg has no fully covered pixels");
        for pixel in opaque {
            assert_eq!(pixel[..3], [color.r, color.g, color.b]);
        }
    }

    #[test]
    fn variants_pick_the_glyph_and_color_for_each_state() {
        let red = IconColor {
            r: 239,
            g: 68,
            b: 68,
        };
        let light_red = IconColor {
            r: 248,
            g: 113,
            b: 113,
        };
        let black = IconColor { r: 0, g: 0, b: 0 };
        let white = IconColor {
            r: 255,
            g: 255,
            b: 255,
        };
        let cases = [
            (popup_mic(true, Theme::Light), Glyph::MicOff, red),
            (popup_mic(false, Theme::Light), Glyph::Mic, black),
            (popup_mic(true, Theme::Dark), Glyph::MicOff, light_red),
            (popup_mic(false, Theme::Dark), Glyph::Mic, white),
            (popup_camera(true, Theme::Light), Glyph::Video, black),
            (popup_camera(false, Theme::Light), Glyph::VideoOff, red),
            (popup_camera(true, Theme::Dark), Glyph::Video, white),
            (popup_camera(false, Theme::Dark), Glyph::VideoOff, light_red),
        ];
        for (variant, glyph, color) in cases {
            assert_eq!(variant, IconVariant { glyph, color });
        }

        let tray_cases = [
            (true, Glyph::MicOff, red, false),
            (false, Glyph::Mic, black, true),
        ];
        for (muted, glyph, color, is_template) in tray_cases {
            let expected = TrayImage {
                variant: IconVariant { glyph, color },
                is_template,
            };
            assert_eq!(tray_mic(muted), expected, "muted {muted}");
        }
    }

    #[test]
    fn rendered_reuses_the_first_render_of_each_variant() {
        let muted = popup_mic(true, Theme::Dark);
        let first = muted.rendered().expect("muted mic renders");
        let again = muted.rendered().expect("muted mic renders");
        assert!(Arc::ptr_eq(&first, &again), "{muted:?} was rendered twice");

        let unmuted = popup_mic(false, Theme::Dark)
            .rendered()
            .expect("unmuted mic renders");
        assert!(
            !Arc::ptr_eq(&first, &unmuted),
            "two variants share a render"
        );
    }

    #[test]
    fn every_variant_renders_a_png_of_its_pixels() {
        for variant in every_variant() {
            let icon = variant
                .rendered()
                .unwrap_or_else(|e| panic!("{variant:?} failed to render: {e:#}"));
            let pixels = icon.width as usize * icon.height as usize * 4;
            assert_eq!(icon.rgba.len(), pixels, "{variant:?}");
            let decoded = image::load_from_memory_with_format(&icon.png, image::ImageFormat::Png)
                .unwrap_or_else(|e| panic!("{variant:?} is not a valid PNG: {e}"))
                .into_rgba8();
            assert_eq!(
                (decoded.width(), decoded.height()),
                (icon.width, icon.height),
                "{variant:?}"
            );
            assert!(
                *decoded.as_raw() == icon.rgba,
                "{variant:?}: the PNG's pixels differ from the RGBA"
            );
        }
    }

    #[test]
    fn rendered_caches_only_the_variants_the_tray_and_popup_show() {
        let mut distinct: Vec<IconVariant> = Vec::new();
        for variant in every_variant() {
            if !distinct.contains(&variant) {
                distinct.push(variant);
            }
        }
        assert_eq!(distinct.len(), 8, "update the variant count in the docs");
        for variant in &distinct {
            variant.rendered().expect("every variant renders");
        }

        let cached = RENDERED.lock();
        assert!(cached.len() <= distinct.len(), "{} cached", cached.len());
        for (variant, _) in cached.iter() {
            assert!(distinct.contains(variant), "{variant:?} is cached");
        }
    }
}
