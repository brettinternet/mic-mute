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

/// Rasterizes an SVG with the given stroke color.
/// Returns un-premultiplied RGBA bytes plus the source dimensions.
pub fn rasterize_svg(svg_bytes: &[u8], color: &IconColor) -> Result<(Vec<u8>, u32, u32)> {
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

    /// Icons the app embeds with `include_bytes!` (`tray.rs`, `popup_content.rs`).
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

    #[test]
    fn every_asset_svg_rasterizes_to_visible_pixels() {
        let paths = asset_svg_paths();
        for icon in EMBEDDED_ICONS {
            assert!(
                paths.iter().any(|path| path.ends_with(icon)),
                "assets/{icon} not found in {paths:?}"
            );
        }

        let color = tray_icon_color(true);
        for path in &paths {
            let name = path.display();
            let svg = std::fs::read(path).expect("SVG should be readable");
            let (rgba, w, h) = rasterize_svg(&svg, &color)
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
}
