//! The scene: an immediate-mode list of drawing commands. It never touches the GPU.

use crate::geom::{Affine, Color, Point, Rect};
use crate::path::{FillRule, Flat, Path, SubPath, Verb};
use crate::stroke::{Cap, Stroke, dash_flat};

/// Handle of an image registered with a renderer (`images().insert(...)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub u32);

/// Identity of a cached layer; chosen by the caller and stable across frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerId(pub u32);

/// How images are sampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sampling {
    Nearest,
    Linear,
}

/// Maximum number of color stops of a gradient.
pub const MAX_STOPS: usize = 16;

/// Color stops of a gradient: a fixed-size value (no allocation). Offsets are 0..=1 and
/// non-decreasing. Colors are interpolated in sRGB with straight alpha, like CSS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stops {
    pub(crate) n: u8,
    pub(crate) offsets: [f32; MAX_STOPS],
    pub(crate) colors: [Color; MAX_STOPS],
}

impl Stops {
    /// From `(offset, color)` pairs; at most 16 are used. Offsets are clamped to 0..=1 and made
    /// non-decreasing.
    pub fn new(stops: &[(f32, Color)]) -> Stops {
        let mut s = Stops { n: 0, offsets: [0.0; MAX_STOPS], colors: [Color::TRANSPARENT; MAX_STOPS] };
        let mut last = 0.0f32;
        for (o, c) in stops.iter().take(MAX_STOPS) {
            let o = o.clamp(0.0, 1.0).max(last);
            last = o;
            s.offsets[s.n as usize] = o;
            s.colors[s.n as usize] = *c;
            s.n += 1;
        }
        s
    }
    /// Two stops: `a` at 0 and `b` at 1.
    pub fn two(a: Color, b: Color) -> Stops {
        Stops::new(&[(0.0, a), (1.0, b)])
    }
    pub fn len(&self) -> usize {
        self.n as usize
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    /// The color at position `t` (straight alpha, sRGB).
    pub fn color_at(&self, t: f32) -> Color {
        let n = self.n as usize;
        if n == 0 {
            return Color::TRANSPARENT;
        }
        if t <= self.offsets[0] {
            return self.colors[0];
        }
        if t >= self.offsets[n - 1] {
            return self.colors[n - 1];
        }
        for i in 1..n {
            if t <= self.offsets[i] {
                let span = self.offsets[i] - self.offsets[i - 1];
                if span <= 1e-9 {
                    return self.colors[i];
                }
                return self.colors[i - 1].lerp(self.colors[i], (t - self.offsets[i - 1]) / span);
            }
        }
        self.colors[n - 1]
    }
}

/// How a shape is filled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    Solid(Color),
    /// Gradient along `start -> end` (local coordinates at the time of drawing).
    Linear { start: Point, end: Point, stops: Stops },
    Radial { center: Point, radius: f32, stops: Stops },
    /// An image; `transform` maps image pixels to local coordinates. Outside the image the edge
    /// pixels repeat.
    Image { image: ImageId, transform: Affine, sampling: Sampling },
}

impl From<Color> for Paint {
    fn from(c: Color) -> Paint {
        Paint::Solid(c)
    }
}

/// A soft shadow of a rounded rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowStyle {
    /// Blur extent in logical pixels (the Gaussian standard deviation is `blur / 2`).
    pub blur: f32,
    pub offset: Point,
    pub color: Color,
}

/// Range of an arena (verbs + points) holding a path.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct PathRef {
    pub(crate) v0: u32,
    pub(crate) v1: u32,
    pub(crate) p0: u32,
    pub(crate) p1: u32,
    /// Content hash (verbs and points), used as a cache key by the wgpu backend.
    #[allow(dead_code)]
    pub(crate) hash: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Op {
    Rect { rect: Rect, paint: u32 },
    RoundRect { rect: Rect, radius: f32, paint: u32 },
    Ellipse { center: Point, rx: f32, ry: f32, paint: u32 },
    /// One straight segment with the same cap on both ends.
    Line { a: Point, b: Point, width: f32, cap: Cap, paint: u32 },
    /// A polyline with round joins: points in the arena `pts[p0..p1]`.
    Polyline { p0: u32, p1: u32, width: f32, cap: Cap, paint: u32 },
    FillPath { path: PathRef, rule: FillRule, paint: u32 },
    StrokePath { path: PathRef, stroke: u32, paint: u32 },
    Shadow { rect: Rect, radius: f32, style: ShadowStyle },
    Image { image: ImageId, dst: Rect, sampling: Sampling },
    /// Positioned glyphs: range of `Scene::glyphs`, font in `Scene::fonts`.
    Glyphs { run: u32, paint: u32 },
    PushClipRect(Rect),
    PushClipRoundRect { rect: Rect, radius: f32 },
    PushClipPath { path: PathRef, rule: FillRule },
    PopClip,
    PushTransform(Affine),
    PopTransform,
    PushOpacity(f32),
    PopOpacity,
    /// A cached layer: index into `Scene::layers`.
    Layer(u32),
}

pub(crate) struct LayerEntry {
    pub id: LayerId,
    pub size: (f32, f32),
    pub version: u64,
    pub used: bool,
    pub scene: Scene,
}

/// Largest side, in pixels, of a layer texture the scale may ask for.
pub(crate) const LAYER_MAX_PX: f32 = 4096.0;

/// The scale a layer's texture is drawn at, given the transform in effect where it is placed
/// (`current`, which includes the device scale) and the device scale `base` (the scale of the
/// root transform). The texture is never smaller than the device scale, and never larger than
/// [`LAYER_MAX_PX`] on a side unless the device scale itself is larger.
pub(crate) fn layer_scale(current: &crate::geom::Affine, base: f32, size: (f32, f32)) -> f32 {
    let wanted = current.max_scale().max(1e-6);
    let longest = size.0.max(size.1).max(1.0);
    let cap = (LAYER_MAX_PX / longest).max(base);
    wanted.min(cap).max(base.min(cap))
}

/// A positioned glyph in a [`GlyphRun`], in the local coordinates of the scene.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SceneGlyph {
    pub glyph: u16,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone)]
pub(crate) struct GlyphRun {
    pub font: std::sync::Arc<crate::font::FontData>,
    pub size: f32,
    pub g0: u32,
    pub g1: u32,
}

/// A list of drawing commands, recorded each frame. Cheap to reuse: [`Scene::clear`] keeps all
/// allocations. A `Scene` is `Send`: build it on any thread.
#[derive(Default)]
pub struct Scene {
    pub(crate) ops: Vec<Op>,
    pub(crate) paints: Vec<Paint>,
    pub(crate) strokes: Vec<Stroke>,
    pub(crate) verbs: Vec<Verb>,
    pub(crate) pts: Vec<Point>,
    pub(crate) layers: Vec<LayerEntry>,
    pub(crate) runs: Vec<GlyphRun>,
    pub(crate) glyphs: Vec<SceneGlyph>,
    scratch: Flat,
    scratch2: Flat,
    scratch_path: Path,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(mut h: u64, v: u32) -> u64 {
    h ^= v as u64;
    h.wrapping_mul(FNV_PRIME)
}

impl Scene {
    pub fn new() -> Scene {
        Scene::default()
    }

    /// Drop the commands but keep every allocation. Layers not used since the previous `clear`
    /// are released.
    pub fn clear(&mut self) {
        self.ops.clear();
        self.paints.clear();
        self.strokes.clear();
        self.verbs.clear();
        self.pts.clear();
        self.runs.clear();
        self.glyphs.clear();
        self.layers.retain(|l| l.used);
        for l in &mut self.layers {
            l.used = false;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of recorded commands (layer contents not counted).
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    fn paint_id(&mut self, p: Paint) -> u32 {
        if let Some(last) = self.paints.last() {
            if *last == p {
                return (self.paints.len() - 1) as u32;
            }
        }
        self.paints.push(p);
        (self.paints.len() - 1) as u32
    }

    fn add_path(&mut self, verbs: &[Verb], pts: &[Point]) -> PathRef {
        let v0 = self.verbs.len() as u32;
        let p0 = self.pts.len() as u32;
        self.verbs.extend_from_slice(verbs);
        self.pts.extend_from_slice(pts);
        let mut h = FNV_OFFSET;
        for v in verbs {
            h = fnv(h, *v as u32);
        }
        for p in pts {
            h = fnv(h, p.x.to_bits());
            h = fnv(h, p.y.to_bits());
        }
        PathRef { v0, v1: self.verbs.len() as u32, p0, p1: self.pts.len() as u32, hash: h }
    }

    // ---- filling -------------------------------------------------------------------------

    pub fn fill_rect(&mut self, rect: Rect, paint: impl Into<Paint>) {
        let paint = self.paint_id(paint.into());
        self.ops.push(Op::Rect { rect, paint });
    }

    pub fn fill_round_rect(&mut self, rect: Rect, radius: f32, paint: impl Into<Paint>) {
        let paint = self.paint_id(paint.into());
        self.ops.push(Op::RoundRect { rect, radius, paint });
    }

    pub fn fill_circle(&mut self, center: Point, radius: f32, paint: impl Into<Paint>) {
        self.fill_ellipse(center, radius, radius, paint);
    }

    pub fn fill_ellipse(&mut self, center: Point, rx: f32, ry: f32, paint: impl Into<Paint>) {
        let paint = self.paint_id(paint.into());
        self.ops.push(Op::Ellipse { center, rx, ry, paint });
    }

    pub fn fill_path(&mut self, path: &Path, rule: FillRule, paint: impl Into<Paint>) {
        if path.is_empty() {
            return;
        }
        let paint = self.paint_id(paint.into());
        let path = self.add_path(&path.verbs, &path.pts);
        self.ops.push(Op::FillPath { path, rule, paint });
    }

    // ---- stroking ------------------------------------------------------------------------

    /// A straight line. Dashes are applied here.
    pub fn stroke_line(&mut self, a: Point, b: Point, stroke: &Stroke, paint: impl Into<Paint>) {
        let paint = self.paint_id(paint.into());
        match stroke.dash {
            None => self.ops.push(Op::Line { a, b, width: stroke.width, cap: stroke.cap, paint }),
            Some(dash) => {
                let mut src = std::mem::take(&mut self.scratch);
                let mut dashed = std::mem::take(&mut self.scratch2);
                src.clear();
                src.pts.extend_from_slice(&[a, b]);
                src.subs.push(SubPath { start: 0, end: 2, closed: false });
                dash_flat(&src, &dash, &mut dashed);
                for s in &dashed.subs {
                    let p = dashed.sub_points(s);
                    if p.len() >= 2 {
                        self.ops.push(Op::Line { a: p[0], b: p[p.len() - 1], width: stroke.width, cap: stroke.cap, paint });
                    }
                }
                self.scratch = src;
                self.scratch2 = dashed;
            }
        }
    }

    /// A polyline. Interior joins are always round (so that thin changing lines such as spectra
    /// stay cheap on the GPU); `stroke.join` is ignored. Use `stroke_path` for other joins.
    pub fn stroke_polyline(&mut self, points: &[Point], stroke: &Stroke, paint: impl Into<Paint>) {
        if points.len() < 2 {
            return;
        }
        let paint = self.paint_id(paint.into());
        match stroke.dash {
            None => {
                let p0 = self.pts.len() as u32;
                self.pts.extend_from_slice(points);
                self.ops.push(Op::Polyline { p0, p1: self.pts.len() as u32, width: stroke.width, cap: stroke.cap, paint });
            }
            Some(dash) => {
                let mut src = std::mem::take(&mut self.scratch);
                let mut dashed = std::mem::take(&mut self.scratch2);
                src.clear();
                src.pts.extend_from_slice(points);
                src.subs.push(SubPath { start: 0, end: points.len() as u32, closed: false });
                dash_flat(&src, &dash, &mut dashed);
                for s in &dashed.subs {
                    let p = dashed.sub_points(s);
                    if p.len() >= 2 {
                        let p0 = self.pts.len() as u32;
                        self.pts.extend_from_slice(p);
                        self.ops.push(Op::Polyline { p0, p1: self.pts.len() as u32, width: stroke.width, cap: stroke.cap, paint });
                    }
                }
                self.scratch = src;
                self.scratch2 = dashed;
            }
        }
    }

    /// Stroke a path (joins, caps and dashes as in `stroke`). Rendered through the mask cache.
    pub fn stroke_path(&mut self, path: &Path, stroke: &Stroke, paint: impl Into<Paint>) {
        if path.is_empty() {
            return;
        }
        let paint = self.paint_id(paint.into());
        self.strokes.push(*stroke);
        let sid = (self.strokes.len() - 1) as u32;
        let path = self.add_path(&path.verbs, &path.pts);
        self.ops.push(Op::StrokePath { path, stroke: sid, paint });
    }

    pub fn stroke_rect(&mut self, rect: Rect, stroke: &Stroke, paint: impl Into<Paint>) {
        let mut p = std::mem::take(&mut self.scratch_path);
        p.clear();
        p.rect(rect);
        self.stroke_path(&p, stroke, paint);
        self.scratch_path = p;
    }

    pub fn stroke_round_rect(&mut self, rect: Rect, radius: f32, stroke: &Stroke, paint: impl Into<Paint>) {
        let mut p = std::mem::take(&mut self.scratch_path);
        p.clear();
        p.round_rect(rect, radius);
        self.stroke_path(&p, stroke, paint);
        self.scratch_path = p;
    }

    pub fn stroke_circle(&mut self, center: Point, radius: f32, stroke: &Stroke, paint: impl Into<Paint>) {
        let mut p = std::mem::take(&mut self.scratch_path);
        p.clear();
        p.circle(center, radius);
        self.stroke_path(&p, stroke, paint);
        self.scratch_path = p;
    }

    // ---- text ------------------------------------------------------------------------------

    /// Draw a laid-out text with its top-left corner at `origin`.
    pub fn text(&mut self, layout: &crate::text::Layout, origin: Point, paint: impl Into<Paint>) {
        let paint = self.paint_id(paint.into());
        let gl = layout.glyphs();
        let mut i = 0;
        while i < gl.len() {
            let f = gl[i].font;
            let g0 = self.glyphs.len() as u32;
            let mut j = i;
            while j < gl.len() && gl[j].font == f {
                self.glyphs.push(SceneGlyph { glyph: gl[j].glyph, x: origin.x + gl[j].x, y: origin.y + gl[j].y });
                j += 1;
            }
            self.runs.push(GlyphRun { font: layout.fonts()[f as usize].clone(), size: layout.font_size(), g0, g1: self.glyphs.len() as u32 });
            self.ops.push(Op::Glyphs { run: (self.runs.len() - 1) as u32, paint });
            i = j;
        }
    }

    // ---- shadow, images --------------------------------------------------------------------

    /// A soft shadow of a rounded rectangle (draw it before the shape that casts it).
    pub fn shadow(&mut self, rect: Rect, radius: f32, style: ShadowStyle) {
        self.ops.push(Op::Shadow { rect, radius, style });
    }

    /// Draw an image stretched over `dst` (bilinear).
    pub fn image(&mut self, image: ImageId, dst: Rect) {
        self.ops.push(Op::Image { image, dst, sampling: Sampling::Linear });
    }

    /// Draw an image stretched over `dst` with the given sampling.
    pub fn image_sampled(&mut self, image: ImageId, dst: Rect, sampling: Sampling) {
        self.ops.push(Op::Image { image, dst, sampling });
    }

    // ---- state -----------------------------------------------------------------------------

    /// Clip to an axis-aligned rectangle (in the current transform; with a rotation the clip
    /// becomes a path clip).
    ///
    /// With a scale and translation only, the clip rectangle is rounded to whole device pixels
    /// (both backends do this, so their pictures agree). The edge is not anti-aliased, and a
    /// clip that moves by fractions of a pixel (a scrolling list, a sliding panel) jumps by a
    /// whole pixel at a time.
    pub fn push_clip_rect(&mut self, rect: Rect) {
        self.ops.push(Op::PushClipRect(rect));
    }

    pub fn push_clip_round_rect(&mut self, rect: Rect, radius: f32) {
        self.ops.push(Op::PushClipRoundRect { rect, radius });
    }

    /// Clip to an arbitrary path. Slow on the GPU (it renders the clipped content to a texture
    /// first); prefer rectangle and rounded-rectangle clips.
    pub fn push_clip_path(&mut self, path: &Path, rule: FillRule) {
        let path = self.add_path(&path.verbs, &path.pts);
        self.ops.push(Op::PushClipPath { path, rule });
    }

    pub fn pop_clip(&mut self) {
        self.ops.push(Op::PopClip);
    }

    /// Everything drawn until `pop_transform` is transformed by `t` (applied before the
    /// transforms already in effect).
    pub fn push_transform(&mut self, t: Affine) {
        self.ops.push(Op::PushTransform(t));
    }

    pub fn pop_transform(&mut self) {
        self.ops.push(Op::PopTransform);
    }

    /// Start a group drawn as a whole with the given opacity (0..=1).
    pub fn push_opacity(&mut self, opacity: f32) {
        self.ops.push(Op::PushOpacity(opacity.clamp(0.0, 1.0)));
    }

    pub fn pop_opacity(&mut self) {
        self.ops.push(Op::PopOpacity);
    }

    /// A cached layer of `size` logical pixels (its content is in local coordinates
    /// `0..size`; it is placed by the current transform). `f` runs only when `version` (or
    /// `size`) differs from the last time this `id` was recorded; otherwise the renderer reuses
    /// the texture it drew earlier.
    ///
    /// The texture is drawn at the scale of the transform in effect where the layer is placed
    /// (so a layer under `push_transform` with a zoom is sharp), up to 4096 pixels on a side or
    /// the device scale if that is larger. A layer placed at a fractional offset is resampled
    /// with linear filtering, so its edges look soft there (the same as any texture).
    pub fn layer(&mut self, id: LayerId, size: (f32, f32), version: u64, f: impl FnOnce(&mut Scene)) {
        let idx = match self.layers.iter().position(|l| l.id == id) {
            Some(i) => i,
            None => {
                self.layers.push(LayerEntry { id, size: (-1.0, -1.0), version: 0, used: false, scene: Scene::new() });
                self.layers.len() - 1
            }
        };
        let fresh = {
            let l = &self.layers[idx];
            l.size != size || l.version != version
        };
        if fresh {
            let mut sub = std::mem::take(&mut self.layers[idx].scene);
            sub.clear();
            f(&mut sub);
            let l = &mut self.layers[idx];
            l.scene = sub;
            l.size = size;
            l.version = version;
        }
        self.layers[idx].used = true;
        self.ops.push(Op::Layer(idx as u32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Scene>();
    }

    #[test]
    fn identical_paints_are_shared() {
        let mut s = Scene::new();
        s.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color::WHITE);
        s.fill_rect(Rect::new(1.0, 0.0, 1.0, 1.0), Color::WHITE);
        assert_eq!(s.paints.len(), 1);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn layer_closure_runs_only_when_the_version_changes() {
        let mut s = Scene::new();
        let mut runs = 0;
        for version in [1u64, 1, 2] {
            s.clear();
            s.layer(LayerId(7), (10.0, 10.0), version, |l| {
                runs += 1;
                l.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::BLACK);
            });
        }
        assert_eq!(runs, 2);
        s.clear();
        s.clear();
        assert!(s.layers.is_empty(), "unused layers are released");
    }

    #[test]
    fn dashed_line_becomes_several_segments() {
        let mut s = Scene::new();
        s.stroke_line(
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            &Stroke::width(1.0).with_dash(crate::stroke::Dash::new(2.0, 2.0)),
            Color::BLACK,
        );
        assert_eq!(s.len(), 3);
    }
}
