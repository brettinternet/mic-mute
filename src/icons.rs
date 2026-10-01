use anyhow::{Context, Result};
use tao::window::Theme;

pub struct IconColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub fn popup_icon_color(muted: bool, theme: Theme) -> IconColor {
    match theme {
        Theme::Light if muted => IconColor {
            r: 239,
            g: 68,
            b: 68,
        }, // #ef4444
        Theme::Light => IconColor { r: 0, g: 0, b: 0 },
        Theme::Dark if muted => IconColor {
            r: 248,
            g: 113,
            b: 113,
        }, // #f87171
        _ => IconColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }
}

pub fn tray_icon_color(muted: bool) -> IconColor {
    if muted {
        IconColor {
            r: 239,
            g: 68,
            b: 68,
        } // #ef4444
    } else {
        IconColor {
            r: 255,
            g: 255,
            b: 255,
        }
    }
}

/// Pixels per SVG unit. Callers size the image in points from its aspect ratio,
/// so rendering at 3x keeps icons sharp on Retina displays.
const RENDER_SCALE: f32 = 3.0;

/// Rasterizes an SVG with the given stroke color.
/// Returns un-premultiplied RGBA bytes plus the pixel dimensions, which are
/// `RENDER_SCALE` times the SVG's own size.
pub fn rasterize_svg(svg_bytes: &[u8], color: &IconColor) -> Result<(Vec<u8>, u32, u32)> {
    rasterize_svg_scaled(svg_bytes, color, 1.0)
}

/// Like `rasterize_svg`, but draws the artwork at `scale` of its size, centred
/// on a canvas of the original dimensions.
pub fn rasterize_svg_scaled(
    svg_bytes: &[u8],
    color: &IconColor,
    scale: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    let svg_str = std::str::from_utf8(svg_bytes).context("SVG is not valid UTF-8")?;
    let colored = svg_str.replacen(
        "<svg ",
        &format!("<svg stroke=\"rgb({},{},{})\" ", color.r, color.g, color.b),
        1,
    );

    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(&colored, &options).context("Failed to parse SVG")?;
    let size = tree.size();
    let width = size.width() * RENDER_SCALE;
    let height = size.height() * RENDER_SCALE;
    let w = width.round() as u32;
    let h = height.round() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("Failed to allocate pixmap")?;
    let transform = resvg::tiny_skia::Transform::from_translate(
        width * (1.0 - scale) / 2.0,
        height * (1.0 - scale) / 2.0,
    )
    .pre_scale(RENDER_SCALE * scale, RENDER_SCALE * scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

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

    #[test]
    fn rasterizes_at_render_scale() {
        let svg = include_bytes!("../assets/mic.svg");
        let color = IconColor { r: 0, g: 0, b: 0 };

        let (rgba, w, h) = rasterize_svg_scaled(svg, &color, 0.9).unwrap();

        assert_eq!((w, h), (72, 72));
        assert_eq!(rgba.len(), 72 * 72 * 4);
    }
}
