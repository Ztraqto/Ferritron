//! The CPU backend: draws a [`Scene`] into an RGBA8 buffer.
//!
//! It is the reference for the pictures the GPU backend must match, and a stand-in where there
//! is no GPU (CI, screenshots).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::geom::{Affine, IRect, Point, Rect};
use crate::glyph::{self, GlyphKey};
use crate::image::{ImageStore, MipLevel};
use crate::path::{FillRule, Flat, Path, SubPath, flatten_raw};
use crate::raster::{Mask, Rasterizer};
use crate::render::RenderOptions;
use crate::scene::{Op, Paint, Sampling, Scene, Stops};
use crate::shade;
use crate::stroke::{Join, Stroke, StrokeScratch, dash_flat, stroke_polys_with};

/// Flattening tolerance in device pixels.
const TOL: f32 = 0.1;

/// A rendered picture: RGBA8, sRGB, straight (non-premultiplied) alpha, rows top to bottom.
#[derive(Clone, Debug, Default)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RgbaImage {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    /// Write the picture as a PNG file.
    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut enc = png::Encoder::new(file, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(std::io::Error::other)?;
        w.write_image_data(&self.data).map_err(std::io::Error::other)
    }

    /// Read a PNG file (8-bit RGBA or RGB) into an image.
    pub fn load_png(path: impl AsRef<std::path::Path>) -> std::io::Result<RgbaImage> {
        let file = std::io::BufReader::new(std::fs::File::open(path)?);
        let dec = png::Decoder::new(file);
        let mut reader = dec.read_info().map_err(std::io::Error::other)?;
        let mut buf = vec![0u8; reader.output_buffer_size().ok_or_else(|| std::io::Error::other("png too large"))?];
        let info = reader.next_frame(&mut buf).map_err(std::io::Error::other)?;
        buf.truncate(info.buffer_size());
        let data = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            other => return Err(std::io::Error::other(format!("unsupported png color type {other:?}"))),
        };
        Ok(RgbaImage { width: info.width, height: info.height, data })
    }
}

/// Premultiplied float surface of the whole picture (`full_w` x `full_h`). Only a window of it
/// is stored: `(ox, oy)` and `(w, h)`, which grows to cover what is drawn into it (`ensure`).
/// A root or layer surface stores all of it; an opacity group starts empty and stores what it
/// draws. `dirty` is the bounding box of every pixel written since the surface was last cleared:
/// an opacity group only composites and clears that part.
struct Surface {
    full_w: i32,
    full_h: i32,
    ox: i32,
    oy: i32,
    w: i32,
    h: i32,
    px: Vec<[f32; 4]>,
    dirty: IRect,
    /// The frame the surface was last taken from the group pool.
    last_used: u64,
}

impl Surface {
    fn new(w: i32, h: i32) -> Surface {
        Surface { full_w: w, full_h: h, ox: 0, oy: 0, w, h, px: vec![[0.0; 4]; (w.max(0) * h.max(0)) as usize], dirty: IRect::new(0, 0, 0, 0), last_used: 0 }
    }
    /// An opacity group of a `full_w` x `full_h` picture: stores nothing until it draws.
    fn group(full_w: i32, full_h: i32) -> Surface {
        Surface { full_w, full_h, ox: 0, oy: 0, w: 0, h: 0, px: Vec::new(), dirty: IRect::new(0, 0, 0, 0), last_used: 0 }
    }
    /// The whole picture.
    fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.full_w, self.full_h)
    }
    /// Index in `px` of the stored pixel `(x, y)` (it must be inside the window).
    fn index(&self, x: i32, y: i32) -> usize {
        ((y - self.oy) * self.w + (x - self.ox)) as usize
    }
    /// Store the pixels of `area` (clipped to the picture): the window grows to cover it, and
    /// what was stored keeps its place. Growing is the only allocation.
    fn ensure(&mut self, area: &IRect) {
        let a = area.intersect(&self.bounds());
        if a.is_empty() {
            return;
        }
        let stored = self.w > 0 && self.h > 0;
        if stored && a.x0 >= self.ox && a.y0 >= self.oy && a.x1 <= self.ox + self.w && a.y1 <= self.oy + self.h {
            return;
        }
        let win = if stored {
            IRect::new(self.ox.min(a.x0), self.oy.min(a.y0), (self.ox + self.w).max(a.x1), (self.oy + self.h).max(a.y1))
        } else {
            a
        };
        let (nw, nh) = (win.x1 - win.x0, win.y1 - win.y0);
        if stored {
            let mut px = vec![[0.0; 4]; (nw as usize) * (nh as usize)];
            for y in self.oy..self.oy + self.h {
                for x in self.ox..self.ox + self.w {
                    px[((y - win.y0) * nw + (x - win.x0)) as usize] = self.px[self.index(x, y)];
                }
            }
            self.px = px;
        } else {
            // nothing stored yet: the buffer (if any) is reused, zeroed
            self.px.clear();
            self.px.resize((nw as usize) * (nh as usize), [0.0; 4]);
        }
        self.ox = win.x0;
        self.oy = win.y0;
        self.w = nw;
        self.h = nh;
    }
    /// Mark `r` (already inside the surface) as written.
    fn touch(&mut self, r: &IRect) {
        if r.is_empty() {
            return;
        }
        self.dirty = if self.dirty.is_empty() {
            *r
        } else {
            IRect::new(self.dirty.x0.min(r.x0), self.dirty.y0.min(r.y0), self.dirty.x1.max(r.x1), self.dirty.y1.max(r.y1))
        };
    }
    /// Make this surface `w` x `h` and clear it to `fill` (its buffer is reused when the size
    /// does not change). The whole picture is stored.
    fn reset(&mut self, w: i32, h: i32, fill: [f32; 4]) {
        self.full_w = w;
        self.full_h = h;
        self.ox = 0;
        self.oy = 0;
        self.w = w;
        self.h = h;
        self.px.clear();
        self.px.resize((w.max(0) as usize) * (h.max(0) as usize), fill);
        self.dirty = IRect::new(0, 0, 0, 0);
    }
    /// Store nothing again, keeping the buffer: the pixels of a group are all clear when it is
    /// composited, so the next picture starts with an empty window.
    fn start_group(&mut self) {
        self.ox = 0;
        self.oy = 0;
        self.w = 0;
        self.h = 0;
        self.dirty = IRect::new(0, 0, 0, 0);
    }
    /// Give the stored pixels back (a group that has not been used for a while).
    fn release(&mut self) {
        self.px = Vec::new();
        self.ox = 0;
        self.oy = 0;
        self.w = 0;
        self.h = 0;
        self.dirty = IRect::new(0, 0, 0, 0);
    }
}

/// Largest side (in pixels) a picture may have. Larger sizes are refused, not truncated.
pub const MAX_SIDE: u32 = 16384;

/// Byte layout of the output of [`CpuRenderer::render_into`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Straight (non-premultiplied) alpha, bytes R G B A. Same as [`RgbaImage`].
    Rgba8,
    /// Premultiplied alpha, bytes B G R A: the format a Windows window (GDI / DIB) takes.
    Bgra8Premultiplied,
}

/// Why [`CpuRenderer::render_into`] refused a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// A side is larger than [`MAX_SIDE`] or the picture does not fit in memory.
    TooLarge,
    /// The output buffer is not `width * height * 4` bytes long.
    BufferSize { expected: usize, actual: usize },
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::TooLarge => write!(f, "picture too large: each side must be at most {MAX_SIDE} pixels"),
            RenderError::BufferSize { expected, actual } => write!(f, "output buffer is {actual} bytes, expected {expected}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Byte count of a picture of `size_px` (4 bytes per pixel), or `Err` when a side is too large.
fn picture_len(size_px: (u32, u32)) -> Result<usize, RenderError> {
    if size_px.0 > MAX_SIDE || size_px.1 > MAX_SIDE {
        return Err(RenderError::TooLarge);
    }
    // usize arithmetic: MAX_SIDE^2 * 4 fits in 64 bits, so this cannot overflow on 64-bit targets
    let px = (size_px.0 as usize) * (size_px.1 as usize);
    px.checked_mul(4).ok_or(RenderError::TooLarge)
}

#[derive(Clone)]
struct Clip {
    bounds: IRect,
    mask: Option<Arc<Mask>>,
}

struct State {
    xf: Vec<Affine>,
    clips: Vec<Clip>,
}

impl State {
    fn cur(&self) -> Affine {
        *self.xf.last().unwrap()
    }
    fn clip(&self) -> &Clip {
        self.clips.last().unwrap()
    }
}

enum Source<'a> {
    U8 { data: &'a [u8], w: u32, h: u32 },
    F32 { data: &'a [[f32; 4]], w: u32, h: u32 },
}

enum Shader<'a> {
    Solid([f32; 4]),
    Linear { inv: Affine, start: Point, delta: Point, inv_len2: f32, stops: &'a Stops },
    Radial { inv: Affine, center: Point, radius: f32, stops: &'a Stops },
    /// `lod`: the mip level of detail (0 for the base image); `mips`: levels 1.. of the image.
    Image { inv: Affine, src: Source<'a>, mips: &'a [MipLevel], lod: f32, sampling: Sampling },
}

/// The paint's mip level of detail for the transform `inv` (device to texels): log2 of the
/// texels a device pixel covers along the longer axis, never below 0. Bilinear images only.
fn image_lod(inv: &Affine, sampling: Sampling) -> f32 {
    if sampling != Sampling::Linear {
        return 0.0;
    }
    let m = inv.0;
    let rho = (m[0] * m[0] + m[1] * m[1]).sqrt().max((m[2] * m[2] + m[3] * m[3]).sqrt());
    if rho > 1.0 { rho.log2() } else { 0.0 }
}

fn image_shader<'a>(inv: Affine, img: &'a crate::image::StoredImage, sampling: Sampling) -> Shader<'a> {
    let lod = image_lod(&inv, sampling);
    Shader::Image { inv, src: Source::U8 { data: &img.premul, w: img.width, h: img.height }, mips: &img.mips, lod, sampling }
}

impl Shader<'_> {
    fn shade(&self, x: f32, y: f32) -> [f32; 4] {
        match self {
            Shader::Solid(c) => *c,
            Shader::Linear { inv, start, delta, inv_len2, stops } => {
                let p = inv.apply(Point::new(x, y));
                let t = ((p - *start).dot(*delta) * inv_len2).clamp(0.0, 1.0);
                stops.color_at(t).premul()
            }
            Shader::Radial { inv, center, radius, stops } => {
                let p = inv.apply(Point::new(x, y));
                let t = ((p - *center).length() / radius.max(1e-6)).clamp(0.0, 1.0);
                stops.color_at(t).premul()
            }
            Shader::Image { inv, src, mips, lod, sampling } => {
                let p = inv.apply(Point::new(x, y));
                sample_lod(src, mips, p.x, p.y, *sampling, *lod)
            }
        }
    }
}

fn texel(src: &Source<'_>, x: i32, y: i32) -> [f32; 4] {
    match src {
        Source::U8 { data, w, h } => {
            let x = x.clamp(0, *w as i32 - 1) as usize;
            let y = y.clamp(0, *h as i32 - 1) as usize;
            let i = (y * *w as usize + x) * 4;
            [data[i] as f32 / 255.0, data[i + 1] as f32 / 255.0, data[i + 2] as f32 / 255.0, data[i + 3] as f32 / 255.0]
        }
        Source::F32 { data, w, h } => {
            let x = x.clamp(0, *w as i32 - 1) as usize;
            let y = y.clamp(0, *h as i32 - 1) as usize;
            data[y * *w as usize + x]
        }
    }
}

fn sample(src: &Source<'_>, u: f32, v: f32, sampling: Sampling) -> [f32; 4] {
    match sampling {
        Sampling::Nearest => texel(src, u.floor() as i32, v.floor() as i32),
        Sampling::Linear => {
            let fx = u - 0.5;
            let fy = v - 0.5;
            let x0 = fx.floor();
            let y0 = fy.floor();
            let tx = fx - x0;
            let ty = fy - y0;
            let (x0, y0) = (x0 as i32, y0 as i32);
            let a = texel(src, x0, y0);
            let b = texel(src, x0 + 1, y0);
            let c = texel(src, x0, y0 + 1);
            let d = texel(src, x0 + 1, y0 + 1);
            let mut o = [0.0; 4];
            for i in 0..4 {
                let top = a[i] + (b[i] - a[i]) * tx;
                let bot = c[i] + (d[i] - c[i]) * tx;
                o[i] = top + (bot - top) * ty;
            }
            o
        }
    }
}

fn dims(src: &Source<'_>) -> (u32, u32) {
    match src {
        Source::U8 { w, h, .. } | Source::F32 { w, h, .. } => (*w, *h),
    }
}

/// Bilinear sample of mip level `k` (0 is the base image). The coordinates are in base texels;
/// a level is addressed by the same normalized position the GPU uses.
fn sample_level(src: &Source<'_>, mips: &[MipLevel], k: usize, u: f32, v: f32) -> [f32; 4] {
    if k == 0 {
        return sample(src, u, v, Sampling::Linear);
    }
    let (w, h, data) = &mips[k - 1];
    let (w0, h0) = dims(src);
    let level = Source::U8 { data: data.as_slice(), w: *w, h: *h };
    sample(&level, u * *w as f32 / w0 as f32, v * *h as f32 / h0 as f32, Sampling::Linear)
}

/// `sample` with a level of detail: the two levels around `lod` are blended (trilinear), as the
/// GPU's linear mipmap filter does. Nearest sampling and `lod <= 0` use the base image.
fn sample_lod(src: &Source<'_>, mips: &[MipLevel], u: f32, v: f32, sampling: Sampling, lod: f32) -> [f32; 4] {
    if lod <= 0.0 || mips.is_empty() || sampling == Sampling::Nearest {
        return sample(src, u, v, sampling);
    }
    let lod = lod.min(mips.len() as f32);
    let k = lod.floor() as usize;
    let t = lod - k as f32;
    let a = sample_level(src, mips, k, u, v);
    if t <= 0.0 {
        return a;
    }
    let b = sample_level(src, mips, k + 1, u, v);
    let mut o = [0.0; 4];
    for i in 0..4 {
        o[i] = a[i] + (b[i] - a[i]) * t;
    }
    o
}

struct CachedLayer {
    id: crate::scene::LayerId,
    version: u64,
    size: (f32, f32),
    scale_bits: u32,
    surf: Surface,
    used: bool,
}

/// Budget of the glyph masks the CPU renderer keeps (bytes of coverage data).
const GLYPH_CACHE_BYTES: usize = 32 << 20;
/// Budget of the shadow coverage maps the CPU renderer keeps.
const SHADOW_CACHE_BYTES: usize = 32 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum GlyphCacheKey {
    /// A glyph drawn upright: the sub-pixel position is in the key, the mask is relative to
    /// the glyph origin.
    Upright(GlyphKey),
    /// A glyph drawn as an outline (rotated, mirrored or scaled unevenly): the transform and
    /// the target size are in the key, the mask is in device pixels.
    Outline { font: u64, glyph: u16, size: u32, origin: [u32; 2], xf: [u32; 6], target: (u32, u32) },
}

/// What a cached value weighs, in bytes.
trait Costed {
    fn cost(&self) -> usize;
}

impl Costed for Mask {
    fn cost(&self) -> usize {
        self.data.len() + 64
    }
}

/// Values kept under a byte budget. Once over it, the oldest entries go first.
struct BudgetCache<K, V> {
    map: HashMap<K, Arc<V>>,
    order: VecDeque<K>,
    bytes: usize,
    budget: usize,
}

impl<K: Copy + Eq + std::hash::Hash, V: Costed> BudgetCache<K, V> {
    fn new(budget: usize) -> Self {
        BudgetCache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, budget }
    }

    fn get(&self, key: &K) -> Option<Arc<V>> {
        self.map.get(key).cloned()
    }

    /// Store `value` under `key` (the key must not be cached yet) and return it.
    fn insert(&mut self, key: K, value: V) -> Arc<V> {
        let value = Arc::new(value);
        self.bytes += value.cost();
        self.map.insert(key, value.clone());
        self.order.push_back(key);
        while self.bytes > self.budget {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(v) = self.map.remove(&old) {
                self.bytes -= v.cost();
            }
        }
        value
    }
}

/// A shadow's coverage over a rectangle of device pixels, relative to its integer origin.
struct CoverageMap {
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
    data: Vec<f32>,
}

impl Costed for CoverageMap {
    fn cost(&self) -> usize {
        self.data.len() * 4 + 64
    }
}

impl CoverageMap {
    fn get(&self, x: i32, y: i32) -> f32 {
        let (lx, ly) = (x - self.x0, y - self.y0);
        if lx < 0 || ly < 0 || lx >= self.w || ly >= self.h {
            0.0
        } else {
            self.data[(ly * self.w + lx) as usize]
        }
    }
}

/// What a shadow's coverage depends on (its integer position is not part of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ShadowKey {
    half: [u32; 2],
    radius: u32,
    sigma: u32,
    scale: [u32; 2],
    /// Fractional part of the device center.
    frac: [u32; 2],
}

/// Draws scenes into memory.
pub struct CpuRenderer {
    rast: Rasterizer,
    flat: Flat,
    flat2: Flat,
    /// Spare room for the dashed stroke (swapped with `flat2`).
    flat3: Flat,
    stroke_scratch: StrokeScratch,
    path: Path,
    /// Second path, for glyph outlines drawn while `path` is in use.
    glyph_path: Path,
    /// Coverage mask scratch, reused by every fill.
    mask: Mask,
    images: ImageStore,
    glyphs: BudgetCache<GlyphCacheKey, Mask>,
    shadows: BudgetCache<ShadowKey, CoverageMap>,
    layers: Vec<CachedLayer>,
    /// The picture surface, kept between frames.
    root: Option<Surface>,
    /// Surfaces for opacity groups (all clear), reused across groups and frames.
    group_pool: Vec<Surface>,
    /// Number of pictures drawn so far (counts the frames a pooled group has been unused).
    frame: u64,
    /// The transform and clip stacks of the last frame, kept for reuse.
    state: Option<State>,
}

impl Default for CpuRenderer {
    fn default() -> Self {
        CpuRenderer::new()
    }
}

impl CpuRenderer {
    pub fn new() -> CpuRenderer {
        CpuRenderer {
            rast: Rasterizer::new(),
            flat: Flat::new(),
            flat2: Flat::new(),
            flat3: Flat::new(),
            stroke_scratch: StrokeScratch::default(),
            path: Path::new(),
            glyph_path: Path::new(),
            mask: Mask::default(),
            images: ImageStore::new(),
            glyphs: BudgetCache::new(GLYPH_CACHE_BYTES),
            shadows: BudgetCache::new(SHADOW_CACHE_BYTES),
            layers: Vec::new(),
            root: None,
            group_pool: Vec::new(),
            frame: 0,
            state: None,
        }
    }

    /// The images this renderer can draw.
    pub fn images(&mut self) -> &mut ImageStore {
        &mut self.images
    }

    /// Draw `scene` on a transparent background (or `clear`), `scale` device pixels per logical
    /// pixel, into a new image of `size_px`.
    ///
    /// Panics when a side is larger than [`MAX_SIDE`]; use [`CpuRenderer::render_into`] to get
    /// an error instead.
    pub fn render(&mut self, scene: &Scene, size_px: (u32, u32), scale: f32) -> RgbaImage {
        self.render_with(scene, &RenderOptions::new(size_px).with_scale(scale))
    }

    /// Like [`CpuRenderer::render`], with the options given.
    ///
    /// Panics when a side is larger than [`MAX_SIDE`]; see [`CpuRenderer::render_into`].
    pub fn render_with(&mut self, scene: &Scene, opts: &RenderOptions) -> RgbaImage {
        let len = picture_len(opts.size_px).unwrap_or_else(|e| panic!("{e}"));
        let mut data = vec![0u8; len];
        self.render_into(scene, opts, &mut data, PixelFormat::Rgba8).unwrap_or_else(|e| panic!("{e}"));
        RgbaImage { width: opts.size_px.0, height: opts.size_px.1, data }
    }

    /// Draw `scene` into `out`, which must hold `width * height * 4` bytes in `format`. Once the
    /// renderer has drawn a picture of this size, a repeat allocates nothing for the surface or
    /// the output (the masks and the layers keep their buffers too).
    pub fn render_into(&mut self, scene: &Scene, opts: &RenderOptions, out: &mut [u8], format: PixelFormat) -> Result<(), RenderError> {
        let len = picture_len(opts.size_px)?;
        if out.len() != len {
            return Err(RenderError::BufferSize { expected: len, actual: out.len() });
        }
        let (w, h) = (opts.size_px.0 as i32, opts.size_px.1 as i32);
        self.frame += 1;
        let clear = match opts.clear {
            Some(c) => c.premul(),
            None => [0.0; 4],
        };
        let mut surf = match self.root.take() {
            Some(mut s) => {
                s.reset(w, h, clear);
                s
            }
            None => {
                let mut s = Surface::new(w, h);
                s.reset(w, h, clear);
                s
            }
        };
        for l in &mut self.layers {
            l.used = false;
        }
        let root_clip = Clip { bounds: surf.bounds(), mask: None };
        let mut st = match self.state.take() {
            Some(mut st) => {
                st.xf.clear();
                st.xf.push(Affine::scale(opts.scale, opts.scale));
                st.clips.clear();
                st.clips.push(root_clip);
                st
            }
            None => State { xf: vec![Affine::scale(opts.scale, opts.scale)], clips: vec![root_clip] },
        };
        let mut pos = 0;
        self.exec(scene, &mut pos, &mut surf, &mut st, false);
        self.state = Some(st);
        self.layers.retain(|l| l.used);
        // a group that has not been used for two pictures gives its pixels back
        let frame = self.frame;
        for g in &mut self.group_pool {
            if frame >= g.last_used + 2 {
                g.release();
            }
        }
        write_pixels(&surf, out, format);
        self.root = Some(surf);
        Ok(())
    }

    fn exec(&mut self, scene: &Scene, pos: &mut usize, surf: &mut Surface, st: &mut State, in_group: bool) {
        while *pos < scene.ops.len() {
            let op = scene.ops[*pos];
            *pos += 1;
            match op {
                Op::Rect { rect, paint } => {
                    let xf = st.cur();
                    self.flat.clear();
                    let p = [
                        xf.apply(Point::new(rect.x, rect.y)),
                        xf.apply(Point::new(rect.right(), rect.y)),
                        xf.apply(Point::new(rect.right(), rect.bottom())),
                        xf.apply(Point::new(rect.x, rect.bottom())),
                    ];
                    self.flat.pts.extend_from_slice(&p);
                    self.flat.subs.push(SubPath { start: 0, end: 4, closed: true });
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::RoundRect { rect, radius, paint } => {
                    self.path.clear();
                    self.path.round_rect(rect, radius);
                    self.path.flatten(&st.cur(), TOL, &mut self.flat);
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::Ellipse { center, rx, ry, paint } => {
                    self.path.clear();
                    self.path.ellipse(center, rx, ry);
                    self.path.flatten(&st.cur(), TOL, &mut self.flat);
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::Line { a, b, width, cap, paint } => {
                    self.flat2.clear();
                    self.flat2.pts.extend_from_slice(&[a, b]);
                    self.flat2.subs.push(SubPath { start: 0, end: 2, closed: false });
                    let stroke = Stroke { width, join: Join::Round, cap, miter_limit: 4.0, dash: None };
                    let xf = st.cur();
                    stroke_polys_with(&self.flat2, &stroke, &xf, TOL / xf.max_scale().max(1e-6), &mut self.flat, &mut self.stroke_scratch);
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::Polyline { p0, p1, width, cap, paint } => {
                    self.flat2.clear();
                    self.flat2.pts.extend_from_slice(&scene.pts[p0 as usize..p1 as usize]);
                    self.flat2.subs.push(SubPath { start: 0, end: self.flat2.pts.len() as u32, closed: false });
                    let stroke = Stroke { width, join: Join::Round, cap, miter_limit: 4.0, dash: None };
                    let xf = st.cur();
                    stroke_polys_with(&self.flat2, &stroke, &xf, TOL / xf.max_scale().max(1e-6), &mut self.flat, &mut self.stroke_scratch);
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::FillPath { path, rule, paint } => {
                    flatten_raw(
                        &scene.verbs[path.v0 as usize..path.v1 as usize],
                        &scene.pts[path.p0 as usize..path.p1 as usize],
                        &st.cur(),
                        TOL,
                        &mut self.flat,
                    );
                    self.draw_flat(surf, st, rule, &scene.paints[paint as usize]);
                }
                Op::StrokePath { path, stroke, paint } => {
                    let stroke = scene.strokes[stroke as usize];
                    let xf = st.cur();
                    let tol_local = TOL / xf.max_scale().max(1e-6);
                    flatten_raw(
                        &scene.verbs[path.v0 as usize..path.v1 as usize],
                        &scene.pts[path.p0 as usize..path.p1 as usize],
                        &Affine::IDENTITY,
                        tol_local,
                        &mut self.flat2,
                    );
                    if let Some(d) = stroke.dash {
                        // Keep both buffers' room: no allocation per dashed stroke.
                        std::mem::swap(&mut self.flat2, &mut self.flat3);
                        dash_flat(&self.flat3, &d, &mut self.flat2);
                    }
                    stroke_polys_with(&self.flat2, &stroke, &xf, tol_local, &mut self.flat, &mut self.stroke_scratch);
                    self.draw_flat(surf, st, FillRule::NonZero, &scene.paints[paint as usize]);
                }
                Op::Shadow { rect, radius, style } => self.draw_shadow(surf, st, rect, radius, style),
                Op::Image { image, dst, sampling } => {
                    let Some(img) = self.images.get(image) else { continue };
                    let t = Affine::scale(dst.w / img.width as f32, dst.h / img.height as f32).then(&Affine::translate(dst.x, dst.y));
                    let xf = st.cur();
                    let Some(inv) = t.then(&xf).invert() else { continue };
                    let shader = image_shader(inv, img, sampling);
                    self.flat.clear();
                    let p = [
                        xf.apply(Point::new(dst.x, dst.y)),
                        xf.apply(Point::new(dst.right(), dst.y)),
                        xf.apply(Point::new(dst.right(), dst.bottom())),
                        xf.apply(Point::new(dst.x, dst.bottom())),
                    ];
                    self.flat.pts.extend_from_slice(&p);
                    self.flat.subs.push(SubPath { start: 0, end: 4, closed: true });
                    let mut mask = std::mem::take(&mut self.mask);
                    self.rast.fill_into(&self.flat, FillRule::NonZero, st.clip().bounds.intersect(&surf.bounds()), &mut mask);
                    composite_mask(surf, &mask, &shader, st.clip());
                    self.mask = mask;
                }
                Op::Glyphs { run, paint } => self.draw_glyphs(scene, surf, st, run as usize, &scene.paints[paint as usize]),
                Op::PushClipRect(r) => {
                    let xf = st.cur();
                    let parent = st.clip().clone();
                    if xf.is_scale_translate() {
                        let b = IRect::round(&xf.map_rect(&r));
                        st.clips.push(Clip { bounds: parent.bounds.intersect(&b), mask: parent.mask });
                    } else {
                        self.path.clear();
                        self.path.rect(r);
                        self.path.flatten(&xf, TOL, &mut self.flat);
                        let c = self.clip_from_flat(FillRule::NonZero, &parent);
                        st.clips.push(c);
                    }
                }
                Op::PushClipRoundRect { rect, radius } => {
                    self.path.clear();
                    self.path.round_rect(rect, radius);
                    self.path.flatten(&st.cur(), TOL, &mut self.flat);
                    let parent = st.clip().clone();
                    let c = self.clip_from_flat(FillRule::NonZero, &parent);
                    st.clips.push(c);
                }
                Op::PushClipPath { path, rule } => {
                    flatten_raw(
                        &scene.verbs[path.v0 as usize..path.v1 as usize],
                        &scene.pts[path.p0 as usize..path.p1 as usize],
                        &st.cur(),
                        TOL,
                        &mut self.flat,
                    );
                    let parent = st.clip().clone();
                    let c = self.clip_from_flat(rule, &parent);
                    st.clips.push(c);
                }
                Op::PopClip => {
                    if st.clips.len() > 1 {
                        st.clips.pop();
                    }
                }
                Op::PushTransform(t) => {
                    let n = t.then(&st.cur());
                    st.xf.push(n);
                }
                Op::PopTransform => {
                    if st.xf.len() > 1 {
                        st.xf.pop();
                    }
                }
                Op::PushOpacity(o) => {
                    if o >= 1.0 {
                        // Full opacity: the content blends straight into this surface (over is
                        // associative), no group needed.
                        self.exec(scene, pos, surf, st, true);
                        continue;
                    }
                    let mut group = match self.group_pool.pop() {
                        Some(g) if g.full_w == surf.full_w && g.full_h == surf.full_h => g,
                        _ => Surface::group(surf.full_w, surf.full_h),
                    };
                    group.last_used = self.frame;
                    group.start_group();
                    self.exec(scene, pos, &mut group, st, true);
                    // Only the part the group wrote can hold anything: composite that, then
                    // clear it again so the surface can be pooled.
                    let r = group.dirty.intersect(&surf.bounds());
                    surf.ensure(&r);
                    for y in r.y0..r.y1 {
                        for x in r.x0..r.x1 {
                            let gi = group.index(x, y);
                            let s = group.px[gi];
                            if s[3] > 0.0 {
                                let di = surf.index(x, y);
                                let d = &mut surf.px[di];
                                let sa = s[3] * o;
                                for c in 0..4 {
                                    d[c] = s[c] * o + d[c] * (1.0 - sa);
                                }
                            }
                            group.px[gi] = [0.0; 4];
                        }
                    }
                    surf.touch(&r);
                    group.dirty = IRect::new(0, 0, 0, 0);
                    self.group_pool.push(group);
                }
                Op::PopOpacity => {
                    if in_group {
                        return;
                    }
                }
                Op::Layer(idx) => self.draw_layer(scene, surf, st, idx as usize),
            }
        }
    }

    /// Rasterize `self.flat` and paint it.
    fn draw_flat(&mut self, surf: &mut Surface, st: &State, rule: FillRule, paint: &Paint) {
        let clip = st.clip();
        let area = clip.bounds.intersect(&surf.bounds());
        if area.is_empty() {
            return;
        }
        let mut mask = std::mem::take(&mut self.mask);
        self.rast.fill_into(&self.flat, rule, area, &mut mask);
        if !mask.is_empty() {
            if let Some(shader) = Self::make_shader(&self.images, paint, &st.cur()) {
                composite_mask(surf, &mask, &shader, clip);
            }
        }
        self.mask = mask;
    }

    fn make_shader<'a>(images: &'a ImageStore, paint: &'a Paint, xf: &Affine) -> Option<Shader<'a>> {
        match paint {
            Paint::Solid(c) => Some(Shader::Solid(c.premul())),
            Paint::Linear { start, end, stops } => {
                let d = *end - *start;
                let l2 = d.dot(d);
                let inv = xf.invert()?;
                if l2 < 1e-12 {
                    return Some(Shader::Solid(stops.color_at(1.0).premul()));
                }
                Some(Shader::Linear { inv, start: *start, delta: d, inv_len2: 1.0 / l2, stops })
            }
            Paint::Radial { center, radius, stops } => Some(Shader::Radial { inv: xf.invert()?, center: *center, radius: *radius, stops }),
            Paint::Image { image, transform, sampling } => {
                let img = images.get(*image)?;
                let inv = transform.then(xf).invert()?;
                Some(image_shader(inv, img, *sampling))
            }
        }
    }

    fn clip_from_flat(&mut self, rule: FillRule, parent: &Clip) -> Clip {
        let area = parent.bounds;
        let m = self.rast.fill(&self.flat, rule, area);
        if m.is_empty() {
            return Clip { bounds: IRect::new(0, 0, 0, 0), mask: None };
        }
        let b = m.bounds().intersect(&area);
        let mut out = Mask { x: b.x0, y: b.y0, w: b.width() as u32, h: b.height() as u32, data: vec![0; (b.width() * b.height()) as usize] };
        for y in b.y0..b.y1 {
            for x in b.x0..b.x1 {
                let mut c = m.get(x, y) as u32;
                if let Some(pm) = &parent.mask {
                    c = (c * pm.get(x, y) as u32 + 127) / 255;
                }
                out.data[((y - b.y0) as u32 * out.w + (x - b.x0) as u32) as usize] = c as u8;
            }
        }
        Clip { bounds: b, mask: Some(Arc::new(out)) }
    }

    fn draw_shadow(&mut self, surf: &mut Surface, st: &State, rect: Rect, radius: f32, style: crate::scene::ShadowStyle) {
        let xf = st.cur();
        let Some(inv) = xf.invert() else { return };
        let sigma = style.blur * 0.5;
        let shifted = rect.translate(style.offset);
        let reach = 3.0 * sigma.max(0.0) + 1.0;
        let dev = xf.map_rect(&shifted.inflate(reach));
        let clip = st.clip();
        let area = IRect::outset(&dev).intersect(&clip.bounds).intersect(&surf.bounds());
        if area.is_empty() {
            return;
        }
        surf.ensure(&area);
        let c = shifted.center();
        let (hx, hy) = (shifted.w * 0.5, shifted.h * 0.5);
        let col = style.color.premul();
        surf.touch(&area);
        let shape = ShadowShape::new(hx, hy, radius, sigma, xf.mean_scale());
        if xf.is_scale_translate() && xf.0[0] > 0.0 && xf.0[3] > 0.0 {
            // The coverage only depends on the shape and on where the device center falls
            // between pixels, so it is computed once per such shadow and reused. The map is
            // built for the whole footprint (not for the clip), and the integer part of the
            // center is moved back out of the key.
            let cdx = xf.0[0] * c.x + xf.0[4];
            let cdy = xf.0[3] * c.y + xf.0[5];
            let (ix0, iy0) = (cdx.floor(), cdy.floor());
            let (fx, fy) = (cdx - ix0, cdy - iy0);
            let key = ShadowKey {
                half: [hx.to_bits(), hy.to_bits()],
                radius: radius.to_bits(),
                sigma: sigma.to_bits(),
                scale: [xf.0[0].to_bits(), xf.0[3].to_bits()],
                frac: [fx.to_bits(), fy.to_bits()],
            };
            let map = match self.shadows.get(&key) {
                Some(m) => m,
                None => {
                    let rel = Affine([xf.0[0], 0.0, 0.0, xf.0[3], fx - xf.0[0] * c.x, fy - xf.0[3] * c.y]);
                    let map = build_coverage(&rel, &shifted.inflate(reach), &shape, c);
                    self.shadows.insert(key, map)
                }
            };
            let (ox, oy) = (ix0 as i32, iy0 as i32);
            for y in area.y0..area.y1 {
                for x in area.x0..area.x1 {
                    let cov = map.get(x - ox, y - oy);
                    let cc = cov * clip_coverage(clip, x, y);
                    if cc > 0.0 {
                        let i = surf.index(x, y);
                        blend(&mut surf.px[i], [col[0] * cc, col[1] * cc, col[2] * cc, col[3] * cc]);
                    }
                }
            }
            return;
        }
        for y in area.y0..area.y1 {
            for x in area.x0..area.x1 {
                let p = inv.apply(Point::new(x as f32 + 0.5, y as f32 + 0.5));
                let cov = shape.coverage(p.x - c.x, p.y - c.y);
                let cc = cov * clip_coverage(clip, x, y);
                if cc > 0.0 {
                    let i = surf.index(x, y);
                    blend(&mut surf.px[i], [col[0] * cc, col[1] * cc, col[2] * cc, col[3] * cc]);
                }
            }
        }
    }

    fn draw_glyphs(&mut self, scene: &Scene, surf: &mut Surface, st: &State, run: usize, paint: &Paint) {
        let r = &scene.runs[run];
        let xf = st.cur();
        let face = r.font.face();
        let Some(shader) = Self::make_shader(&self.images, paint, &xf) else { return };
        let clip = st.clip();
        let area = clip.bounds.intersect(&surf.bounds());
        let upright = xf.is_scale_translate() && xf.0[0] > 0.0 && xf.0[3] > 0.0 && (xf.0[0] - xf.0[3]).abs() < 1e-4;
        let target = (surf.full_w as u32, surf.full_h as u32);
        for g in &scene.glyphs[r.g0 as usize..r.g1 as usize] {
            let dev = xf.apply(Point::new(g.x, g.y));
            if upright {
                let size_px = r.size * xf.0[0];
                let (ix, iy, sub) = glyph::place(dev);
                let key = GlyphKey { font: r.font.uid(), glyph: g.glyph, size: glyph::quantize_size(size_px), sub };
                let ckey = GlyphCacheKey::Upright(key);
                let mask = match self.glyphs.get(&ckey) {
                    Some(m) => m,
                    None => {
                        let m = glyph::rasterize(&r.font, &face, &key, &mut self.rast, &mut self.path, &mut self.flat);
                        self.glyphs.insert(ckey, m)
                    }
                };
                if mask.is_empty() {
                    continue;
                }
                let placed = Mask { x: mask.x + ix, y: mask.y + iy, w: mask.w, h: mask.h, data: Vec::new() };
                composite_mask_ref(surf, &mask, placed.bounds(), &shader, clip, area);
            } else {
                // Cached in device pixels for the whole target (origin and transform are in the
                // key), so the coverage does not depend on the clip it is drawn with.
                let xf_bits = [xf.0[0].to_bits(), xf.0[1].to_bits(), xf.0[2].to_bits(), xf.0[3].to_bits(), xf.0[4].to_bits(), xf.0[5].to_bits()];
                let origin = [g.x.to_bits(), g.y.to_bits()];
                let ckey = GlyphCacheKey::Outline { font: r.font.uid(), glyph: g.glyph, size: r.size.to_bits(), origin, xf: xf_bits, target };
                let mask = match self.glyphs.get(&ckey) {
                    Some(m) => m,
                    None => {
                        self.glyph_path.clear();
                        if !glyph::outline_at(&r.font, &face, g.glyph, r.size, Point::new(g.x, g.y), &mut self.glyph_path) {
                            continue;
                        }
                        self.glyph_path.flatten(&xf, TOL, &mut self.flat);
                        let mut m = Mask::default();
                        self.rast.fill_into(&self.flat, FillRule::NonZero, surf.bounds(), &mut m);
                        self.glyphs.insert(ckey, m)
                    }
                };
                if mask.is_empty() {
                    continue;
                }
                composite_mask_ref(surf, &mask, mask.bounds(), &shader, clip, area);
            }
        }
    }

    fn draw_layer(&mut self, scene: &Scene, surf: &mut Surface, st: &State, idx: usize) {
        let entry = &scene.layers[idx];
        // scale of the layer texture: the transform it is placed with (see `layer_scale`)
        let scale = crate::scene::layer_scale(&st.cur(), st.xf[0].0[0], entry.size);
        let pw = (entry.size.0 * scale).ceil().max(1.0) as i32;
        let ph = (entry.size.1 * scale).ceil().max(1.0) as i32;
        let sbits = scale.to_bits();
        let slot = self.layers.iter().position(|l| l.id == entry.id);
        let fresh = match slot {
            Some(i) => {
                let l = &self.layers[i];
                l.version != entry.version || l.size != entry.size || l.scale_bits != sbits || l.surf.w != pw || l.surf.h != ph
            }
            None => true,
        };
        let slot = if fresh {
            let mut ls = Surface::new(pw, ph);
            let mut lst = State { xf: vec![Affine::scale(scale, scale)], clips: vec![Clip { bounds: ls.bounds(), mask: None }] };
            let mut pos = 0;
            self.exec(&entry.scene, &mut pos, &mut ls, &mut lst, false);
            let cl = CachedLayer { id: entry.id, version: entry.version, size: entry.size, scale_bits: sbits, surf: ls, used: true };
            match slot {
                Some(i) => {
                    self.layers[i] = cl;
                    i
                }
                None => {
                    self.layers.push(cl);
                    self.layers.len() - 1
                }
            }
        } else {
            let i = slot.unwrap();
            self.layers[i].used = true;
            i
        };
        let xf = st.cur();
        let t = Affine::scale(1.0 / scale, 1.0 / scale);
        let Some(inv) = t.then(&xf).invert() else { return };
        let l = &self.layers[slot];
        let shader = Shader::Image { inv, src: Source::F32 { data: &l.surf.px, w: l.surf.w as u32, h: l.surf.h as u32 }, mips: &[], lod: 0.0, sampling: Sampling::Linear };
        let r = Rect::new(0.0, 0.0, entry.size.0, entry.size.1);
        self.flat.clear();
        let p = [
            xf.apply(Point::new(r.x, r.y)),
            xf.apply(Point::new(r.right(), r.y)),
            xf.apply(Point::new(r.right(), r.bottom())),
            xf.apply(Point::new(r.x, r.bottom())),
        ];
        self.flat.pts.extend_from_slice(&p);
        self.flat.subs.push(SubPath { start: 0, end: 4, closed: true });
        let mut mask = std::mem::take(&mut self.mask);
        self.rast.fill_into(&self.flat, FillRule::NonZero, st.clip().bounds.intersect(&surf.bounds()), &mut mask);
        composite_mask(surf, &mask, &shader, st.clip());
        self.mask = mask;
    }
}

/// The coverage rule of a shadow (a blurred rounded rectangle), in its own coordinates with the
/// center at the origin.
struct ShadowShape {
    hx: f32,
    hy: f32,
    radius: f32,
    sigma: f32,
    /// Local scale used for the sharp (unblurred) edge.
    mean_scale: f32,
    /// Blur only changes the coverage within `band` of the edge. Within `inner` of the center
    /// (both axes) the coverage is the constant `interior`; beyond `band` outside it is 0 (to
    /// within 1e-4). Only the band in between is evaluated.
    inner: (f32, f32),
    band: f32,
    interior: f32,
}

impl ShadowShape {
    fn new(hx: f32, hy: f32, radius: f32, sigma: f32, mean_scale: f32) -> ShadowShape {
        let blurred = sigma > 1e-3;
        let band = 4.0 * sigma;
        let corner = radius.min(hx).min(hy).max(0.0);
        // a negative `inner` (a shape too small to have a flat part) matches nothing
        let inner = (hx - corner - band, hy - corner - band);
        let interior = if blurred { shade::blurred_interior(sigma) } else { 0.0 };
        ShadowShape { hx, hy, radius, sigma, mean_scale, inner, band, interior }
    }

    /// Coverage at `(lx, ly)` relative to the center.
    fn coverage(&self, lx: f32, ly: f32) -> f32 {
        if self.sigma > 1e-3 {
            if lx.abs() <= self.inner.0 && ly.abs() <= self.inner.1 {
                self.interior
            } else if shade::round_rect_sdf(lx, ly, self.hx, self.hy, self.radius) > self.band {
                0.0
            } else {
                shade::blurred_round_rect(lx, ly, self.hx, self.hy, self.radius, self.sigma)
            }
        } else {
            let d = shade::round_rect_sdf(lx, ly, self.hx, self.hy, self.radius);
            (0.5 - d * self.mean_scale).clamp(0.0, 1.0)
        }
    }
}

/// The coverage of a shadow over the device pixels of `footprint` under `rel` (a
/// scale-and-translate transform whose origin is the integer corner of the real one).
/// `c` is the shadow's local center.
fn build_coverage(rel: &Affine, footprint: &Rect, shape: &ShadowShape, c: Point) -> CoverageMap {
    let fp = IRect::outset(&rel.map_rect(footprint));
    let (w, h) = (fp.width(), fp.height());
    let mut data = vec![0.0f32; (w as usize) * (h as usize)];
    if let Some(inv) = rel.invert() {
        for y in 0..h {
            for x in 0..w {
                let p = inv.apply(Point::new((fp.x0 + x) as f32 + 0.5, (fp.y0 + y) as f32 + 0.5));
                data[(y * w + x) as usize] = shape.coverage(p.x - c.x, p.y - c.y);
            }
        }
    }
    CoverageMap { x0: fp.x0, y0: fp.y0, w, h, data }
}

/// Convert the premultiplied surface into `out` in `format` (`out` has 4 bytes per pixel).
fn write_pixels(surf: &Surface, out: &mut [u8], format: PixelFormat) {
    fn byte(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }
    for (d, s) in out.chunks_exact_mut(4).zip(&surf.px) {
        let a = s[3];
        if a <= 0.0 {
            d.fill(0);
            continue;
        }
        match format {
            PixelFormat::Rgba8 => {
                d[0] = byte(s[0] / a);
                d[1] = byte(s[1] / a);
                d[2] = byte(s[2] / a);
                d[3] = byte(a);
            }
            PixelFormat::Bgra8Premultiplied => {
                d[0] = byte(s[2]);
                d[1] = byte(s[1]);
                d[2] = byte(s[0]);
                d[3] = byte(a);
            }
        }
    }
}

fn clip_coverage(clip: &Clip, x: i32, y: i32) -> f32 {
    match &clip.mask {
        Some(m) => m.get(x, y) as f32 / 255.0,
        None => 1.0,
    }
}

#[inline]
fn blend(d: &mut [f32; 4], s: [f32; 4]) {
    let inv = 1.0 - s[3];
    for i in 0..4 {
        d[i] = s[i] + d[i] * inv;
    }
}

fn composite_mask(surf: &mut Surface, mask: &Mask, shader: &Shader<'_>, clip: &Clip) {
    if mask.is_empty() {
        return;
    }
    let area = mask.bounds().intersect(&clip.bounds).intersect(&surf.bounds());
    composite_mask_ref(surf, mask, mask.bounds(), shader, clip, area);
}

/// Composite `mask` whose top-left is at `mask_bounds` (it may be offset from `mask.x/y`, used
/// for cached glyph masks) over `area`.
fn composite_mask_ref(surf: &mut Surface, mask: &Mask, mask_bounds: IRect, shader: &Shader<'_>, clip: &Clip, area: IRect) {
    let area = area.intersect(&mask_bounds).intersect(&clip.bounds).intersect(&surf.bounds());
    if area.is_empty() {
        return;
    }
    surf.touch(&area);
    surf.ensure(&area);
    let solid = if let Shader::Solid(c) = shader { Some(*c) } else { None };
    for y in area.y0..area.y1 {
        let mrow = ((y - mask_bounds.y0) as u32 * mask.w) as usize;
        for x in area.x0..area.x1 {
            let m = mask.data[mrow + (x - mask_bounds.x0) as usize];
            if m == 0 {
                continue;
            }
            let mut cov = m as f32 / 255.0;
            if let Some(cm) = &clip.mask {
                cov *= cm.get(x, y) as f32 / 255.0;
                if cov == 0.0 {
                    continue;
                }
            }
            let c = solid.unwrap_or_else(|| shader.shade(x as f32 + 0.5, y as f32 + 0.5));
            let i = surf.index(x, y);
            blend(&mut surf.px[i], [c[0] * cov, c[1] * cov, c[2] * cov, c[3] * cov]);
        }
    }
}
