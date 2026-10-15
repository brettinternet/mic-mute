use anyhow::{bail, Context, Result};
use image::ImageEncoder;
use parking_lot::Mutex;
use resvg::tiny_skia::{Pixmap, Transform};
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

/// The camera-in-use badge: a dot in the top-right corner, where neither mic glyph has ink. Its
/// circle's stroke is as wide as the circle's diameter, which fills it, so the badge stays
/// stroke-only.
const CAMERA_BADGE_SVG: &[u8] = include_bytes!("../assets/camera-badge.svg");

/// Pixels per SVG unit in the popup's icons: 48 px, so its 16 pt icons stay sharp on a 2x
/// display, which shows them at 32 px; the SVGs' own 24 px were scaled up there. AppKit scales
/// them down. The tray's icons keep the SVGs' 24 px.
const POPUP_PIXELS_PER_UNIT: u32 = 2;

/// One icon in one color and resolution, with or without the camera badge. Only `tray_mic`,
/// `popup_mic`, and `popup_camera` build variants: 12 distinct ones, 4 for the tray and 8 for
/// the popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IconVariant {
    glyph: Glyph,
    /// Draws `CAMERA_BADGE_SVG` over the glyph.
    has_camera_badge: bool,
    color: IconColor,
    /// Pixels per SVG unit.
    pixels_per_unit: u32,
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
/// template: red reads on both. A camera in use adds the badge, in the same color.
pub fn tray_mic(muted: bool, camera_in_use: bool) -> TrayImage {
    TrayImage {
        variant: IconVariant {
            glyph: Glyph::mic(muted),
            has_camera_badge: camera_in_use,
            color: if muted { RED } else { BLACK },
            pixels_per_unit: 1,
        },
        is_template: !muted,
    }
}

/// The popup's mic icon: red while muted, else the theme's text color.
pub fn popup_mic(muted: bool, theme: Theme) -> IconVariant {
    IconVariant {
        glyph: Glyph::mic(muted),
        has_camera_badge: false,
        color: popup_icon_color(muted, theme),
        pixels_per_unit: POPUP_PIXELS_PER_UNIT,
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
        has_camera_badge: false,
        color: popup_icon_color(!in_use, theme),
        pixels_per_unit: POPUP_PIXELS_PER_UNIT,
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
/// variant changes. Bounded by the variant count (12 entries of a few KB each), so it never
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
        let glyph = self.glyph.svg();
        let layers: &[&[u8]] = if self.has_camera_badge {
            &[glyph, CAMERA_BADGE_SVG]
        } else {
            &[glyph]
        };
        let (rgba, width, height) = rasterize_layers(layers, &self.color, self.pixels_per_unit)?;
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

/// Parses an SVG with the given stroke color.
fn parse_svg(svg_bytes: &[u8], color: &IconColor) -> Result<resvg::usvg::Tree> {
    let svg_str = std::str::from_utf8(svg_bytes).context("SVG is not valid UTF-8")?;
    let colored = svg_str.replacen(
        "<svg ",
        &format!("<svg stroke=\"rgb({},{},{})\" ", color.r, color.g, color.b),
        1,
    );
    let options = resvg::usvg::Options::default();
    resvg::usvg::Tree::from_str(&colored, &options).context("Failed to parse SVG")
}

/// Pixel dimensions of a parsed SVG.
fn tree_size(tree: &resvg::usvg::Tree) -> (u32, u32) {
    let size = tree.size();
    (size.width() as u32, size.height() as u32)
}

/// Rasterizes SVG layers with the given stroke color at `pixels_per_unit` pixels per SVG unit,
/// each drawn over the ones before it; every layer must have the first one's size. Returns
/// un-premultiplied RGBA bytes plus the pixel size.
fn rasterize_layers(
    layers: &[&[u8]],
    color: &IconColor,
    pixels_per_unit: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let [first, overlays @ ..] = layers else {
        bail!("Failed to rasterize an icon without SVG layers");
    };
    let tree = parse_svg(first, color)?;
    let (w, h) = tree_size(&tree);
    let (width, height) = (w * pixels_per_unit, h * pixels_per_unit);
    let mut pixmap = Pixmap::new(width, height).context("Failed to allocate pixmap")?;
    let scale = pixels_per_unit as f32;
    let transform = Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    for overlay in overlays {
        let tree = parse_svg(overlay, color)?;
        let (overlay_w, overlay_h) = tree_size(&tree);
        if (overlay_w, overlay_h) != (w, h) {
            bail!("Failed to draw a {overlay_w}x{overlay_h} SVG layer over a {w}x{h} one");
        }
        // resvg draws over what the pixmap already holds.
        resvg::render(&tree, transform, &mut pixmap.as_mut());
    }
    Ok((unpremultiply(&pixmap.take()), width, height))
}

/// tiny-skia produces premultiplied RGBA; callers get it un-premultiplied.
fn unpremultiply(premultiplied: &[u8]) -> Vec<u8> {
    let (chunks, _) = premultiplied.as_chunks::<4>();
    chunks
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
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Icons the app embeds with `include_bytes!` (`Glyph::svg`, `CAMERA_BADGE_SVG`).
    const EMBEDDED_ICONS: [&str; 5] = [
        "mic.svg",
        "mic-off.svg",
        "video.svg",
        "video-off.svg",
        "camera-badge.svg",
    ];

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
            for camera_in_use in [true, false] {
                variants.push(tray_mic(state, camera_in_use).variant);
            }
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
            let (rgba, w, h) = rasterize_layers(&[&svg], &RED, 1)
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
    fn rasterize_layers_strokes_opaque_pixels_in_the_requested_color() {
        let color = popup_icon_color(true, Theme::Light);
        let (rgba, _, _) = rasterize_layers(&[include_bytes!("../assets/mic.svg")], &color, 1)
            .expect("mic.svg rasterizes");
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
            let expected = IconVariant {
                glyph,
                has_camera_badge: false,
                color,
                pixels_per_unit: 2,
            };
            assert_eq!(variant, expected);
        }

        let tray_cases = [
            ((true, false), Glyph::MicOff, false, red, false),
            ((true, true), Glyph::MicOff, true, red, false),
            ((false, false), Glyph::Mic, false, black, true),
            ((false, true), Glyph::Mic, true, black, true),
        ];
        for ((muted, camera_in_use), glyph, has_camera_badge, color, is_template) in tray_cases {
            let expected = TrayImage {
                variant: IconVariant {
                    glyph,
                    has_camera_badge,
                    color,
                    pixels_per_unit: 1,
                },
                is_template,
            };
            assert_eq!(
                tray_mic(muted, camera_in_use),
                expected,
                "muted {muted}, camera in use {camera_in_use}"
            );
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
        assert_eq!(distinct.len(), 12, "update the variant count in the docs");
        for variant in &distinct {
            variant.rendered().expect("every variant renders");
        }

        let cached = RENDERED.lock();
        assert!(cached.len() <= distinct.len(), "{} cached", cached.len());
        for (variant, _) in cached.iter() {
            assert!(distinct.contains(variant), "{variant:?} is cached");
        }
    }

    #[test]
    fn popup_icons_have_twice_the_pixels_of_the_tray_icons() {
        let tray = tray_mic(true, false)
            .variant
            .rendered()
            .expect("tray icon renders");
        assert_eq!((tray.width, tray.height), (24, 24));
        for variant in [
            popup_mic(true, Theme::Light),
            popup_camera(true, Theme::Dark),
        ] {
            let icon = variant.rendered().expect("popup icon renders");
            assert_eq!((icon.width, icon.height), (48, 48), "{variant:?}");
        }
    }

    #[test]
    fn a_doubled_render_covers_the_same_shape() {
        let svg: &[u8] = include_bytes!("../assets/mic.svg");
        let (single, w, _) = rasterize_layers(&[svg], &BLACK, 1).expect("1x renders");
        let (double, w2, _) = rasterize_layers(&[svg], &BLACK, 2).expect("2x renders");
        assert_eq!(w2, w * 2);
        // Every pixel fully covered at 1x keeps ink in its 2x2 block at 2x.
        let alpha_at =
            |rgba: &[u8], width: u32, x: u32, y: u32| rgba[((y * width + x) * 4 + 3) as usize];
        let mut covered = 0;
        for y in 0..w {
            for x in 0..w {
                if alpha_at(&single, w, x, y) == u8::MAX {
                    covered += 1;
                    let block = [(0, 0), (1, 0), (0, 1), (1, 1)]
                        .map(|(dx, dy)| alpha_at(&double, w2, 2 * x + dx, 2 * y + dy));
                    assert!(
                        block.iter().any(|&a| a > 0),
                        "({x}, {y}) lost its ink at 2x"
                    );
                }
            }
        }
        assert!(covered > 0, "mic.svg has no fully covered pixels at 1x");
    }

    /// The alpha channel of `layers` rasterized in black.
    fn alpha(layers: &[&[u8]]) -> Vec<u8> {
        let (rgba, _, _) = rasterize_layers(layers, &BLACK, 1).expect("layers rasterize");
        rgba.chunks_exact(4).map(|pixel| pixel[3]).collect()
    }

    #[test]
    fn camera_badge_is_a_solid_dot_clear_of_both_mic_glyphs() {
        let (_, width, _) =
            rasterize_layers(&[CAMERA_BADGE_SVG], &BLACK, 1).expect("badge renders");
        let badge = alpha(&[CAMERA_BADGE_SVG]);
        let position = |index: usize| (index as u32 % width, index as u32 / width);

        // The pixels at least half covered span the dot's diameter. Every pixel whose center
        // lies more than a pixel inside the dot's edge must be fully covered: no hole.
        let covered: Vec<(u32, u32)> = (0..badge.len())
            .filter(|&index| badge[index] >= 128)
            .map(position)
            .collect();
        let x_span = covered.iter().map(|&(x, _)| x);
        let y_span = covered.iter().map(|&(_, y)| y);
        let (min_x, max_x) = (x_span.clone().min().unwrap(), x_span.max().unwrap());
        let (min_y, max_y) = (y_span.clone().min().unwrap(), y_span.max().unwrap());
        let center_x = (min_x + max_x + 1) as f32 / 2.0;
        let center_y = (min_y + max_y + 1) as f32 / 2.0;
        let radius = (max_x + 1 - min_x) as f32 / 2.0;
        assert!(radius >= 2.5, "the badge is only {radius} px wide");
        for (index, &coverage) in badge.iter().enumerate() {
            let (x, y) = position(index);
            let distance = (x as f32 + 0.5 - center_x).hypot(y as f32 + 0.5 - center_y);
            if distance <= radius - 1.0 {
                assert_eq!(coverage, u8::MAX, "hole at ({x}, {y})");
            }
        }

        for glyph in [Glyph::Mic, Glyph::MicOff] {
            let plain = alpha(&[glyph.svg()]);
            let badged = alpha(&[glyph.svg(), CAMERA_BADGE_SVG]);
            for index in 0..badge.len() {
                let (x, y) = position(index);
                assert!(
                    plain[index] == 0 || badge[index] == 0,
                    "{glyph:?} and the badge both ink ({x}, {y})"
                );
                let expected = plain[index].max(badge[index]);
                assert_eq!(badged[index], expected, "{glyph:?} at ({x}, {y})");
            }
        }
    }

    #[test]
    fn rasterize_layers_rejects_missing_and_differently_sized_layers() {
        assert!(rasterize_layers(&[], &BLACK, 1).is_err());

        let small = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none">
            <path d="M1 1h14"/></svg>"#;
        let err = rasterize_layers(&[Glyph::Mic.svg(), small], &BLACK, 1)
            .expect_err("a 16x16 layer over a 24x24 glyph");
        assert!(err.to_string().contains("16x16"), "{err:#}");
    }
}
