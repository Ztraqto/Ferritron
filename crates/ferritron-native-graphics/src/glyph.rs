//! Glyph coverage masks. Glyphs are rasterized by the same exact-area rasterizer as paths.

use crate::font::FontData;
use crate::geom::{Affine, IRect, Point};
use crate::path::{FillRule, Flat, Path};
use crate::raster::{Mask, Rasterizer};

/// Number of sub-pixel positions along x.
pub(crate) const SUBPIXEL_STEPS: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    pub font: u64,
    pub glyph: u16,
    /// Size in 1/16 pixel.
    pub size: u32,
    pub sub: u8,
}

pub(crate) fn quantize_size(px: f32) -> u32 {
    (px * 16.0).round().max(1.0) as u32
}

/// Integer device position and sub-pixel step of a glyph origin. x is quantized to 1/4 pixel,
/// y is snapped to the pixel grid (baselines stay sharp).
pub(crate) fn place(pos: Point) -> (i32, i32, u8) {
    let mut ix = pos.x.floor();
    let mut q = ((pos.x - ix) * SUBPIXEL_STEPS as f32).round() as i32;
    if q >= SUBPIXEL_STEPS {
        q = 0;
        ix += 1.0;
    }
    (ix as i32, pos.y.round() as i32, q as u8)
}

/// Rasterize one glyph; the mask position is relative to the glyph origin.
pub(crate) fn rasterize(
    font: &FontData,
    face: &ttf_parser::Face<'_>,
    key: &GlyphKey,
    rast: &mut Rasterizer,
    path: &mut Path,
    flat: &mut Flat,
) -> Mask {
    path.clear();
    let size = key.size as f32 / 16.0;
    if !font.outline(face, key.glyph, size, path) {
        return Mask::default();
    }
    let dx = key.sub as f32 / SUBPIXEL_STEPS as f32;
    path.flatten(&Affine::translate(dx, 0.0), 0.1, flat);
    rast.fill(flat, FillRule::NonZero, IRect::new(-4096, -4096, 4096, 4096))
}

/// Outline of a glyph at `size` placed with origin `pos` in local coordinates (for glyphs drawn
/// as plain paths, e.g. under rotation).
pub(crate) fn outline_at(font: &FontData, face: &ttf_parser::Face<'_>, glyph: u16, size: f32, pos: Point, path: &mut Path) -> bool {
    let mut tmp = Path::new();
    if !font.outline(face, glyph, size, &mut tmp) {
        return false;
    }
    path.verbs.extend_from_slice(&tmp.verbs);
    path.pts.extend(tmp.pts.iter().map(|p| Point::new(p.x + pos.x, p.y + pos.y)));
    true
}
