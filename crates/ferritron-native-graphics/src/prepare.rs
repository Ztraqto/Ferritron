//! Scene -> GPU work: the CPU side of the wgpu backend.
//!
//! [`Planner::plan`] walks a [`Scene`], keeps masks in the atlas, gradients in the ramp table
//! and layers in their cache, and produces a list of render passes, each with batches of
//! instances for the one shader. It does not touch wgpu, so it can be tested (and measured)
//! without a device, and it allocates nothing once its pools are warm and the scene repeats.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};

use crate::atlas::{Atlas, InsertError, MaskKey};
use crate::geom::{Affine, IRect, Point, Rect};
use crate::glyph::{self, GlyphKey};
use crate::image::ImageStore;
use crate::path::{FillRule, Flat, Path, flatten_raw};
use crate::raster::Rasterizer;
use crate::render::RenderOptions;
use crate::scene::{LayerEntry, Op, Paint, Sampling, Scene, Stops};
use crate::stroke::{Cap, Join, Stroke, StrokeScratch, dash_flat, stroke_polys_with};

/// Flattening tolerance in device pixels.
const TOL: f32 = 0.1;

/// Steps per device pixel for the offset of a mask in its key (quarter pixels).
const SUB_STEPS: f32 = 4.0;
/// Key bit for masks drawn without the target size (see [`Shape`]).
const FREE_BIT: u64 = 1 << 62;
/// Shapes wider or taller than this (device pixels) are cut to the target, as before.
const FREE_MAX: f32 = 4000.0;
/// Whole-pixel parts of a translation beyond this are not split off (no fraction left).
const SPLIT_MAX: f32 = 8.0e6;

/// Keys and placement of one mask. A mask is rasterized in a frame that has the whole part of
/// the translation taken out (`off`, added back when the mask is placed), with the fraction
/// rounded to a quarter pixel. So the same shape at different whole-pixel positions shares one
/// mask, and the mask does not depend on the size of the target (`free`). A shape too big for
/// the atlas is cut to the target instead (`fixed`, the old key: the whole transform and size).
#[derive(Clone, Copy)]
struct Shape {
    free: MaskKey,
    fixed: MaskKey,
    off: (i32, i32),
    /// The transform the shape is flattened with (fraction only).
    xf: Affine,
}

impl Shape {
    /// `free_b` and `fixed_b` are the key fields besides the transform (`fixed_b` includes the
    /// target size).
    fn new(xf: &Affine, kind: u8, a: u64, free_b: u64, fixed_b: u64) -> Shape {
        let m = xf.0;
        let ix = if m[4].abs() < SPLIT_MAX { m[4].floor() } else { 0.0 };
        let iy = if m[5].abs() < SPLIT_MAX { m[5].floor() } else { 0.0 };
        let fx = ((m[4] - ix) * SUB_STEPS).round() / SUB_STEPS;
        let fy = ((m[5] - iy) * SUB_STEPS).round() / SUB_STEPS;
        let local = Affine([m[0], m[1], m[2], m[3], fx, fy]);
        Shape {
            free: MaskKey { kind, a, b: free_b | FREE_BIT, xf: Planner::xf_bits(&local) },
            fixed: MaskKey { kind, a, b: fixed_b, xf: Planner::xf_bits(xf) },
            off: (ix as i32, iy as i32),
            xf: local,
        }
    }
}

pub(crate) const KIND_RRECT: f32 = 0.0;
pub(crate) const KIND_ELLIPSE: f32 = 1.0;
pub(crate) const KIND_LINE: f32 = 2.0;
pub(crate) const KIND_SHADOW: f32 = 3.0;
pub(crate) const KIND_MASK: f32 = 4.0;
pub(crate) const KIND_FULL: f32 = 5.0;

pub(crate) const PAINT_SOLID: f32 = 0.0;
pub(crate) const PAINT_LINEAR: f32 = 1.0;
pub(crate) const PAINT_RADIAL: f32 = 2.0;
pub(crate) const PAINT_IMAGE: f32 = 3.0;

/// One drawn primitive. All instances share one shader; fields are documented in
/// `shader.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Instance {
    pub bbox: [f32; 4],
    pub inv0: [f32; 4],
    pub inv1: [f32; 4],
    pub shape: [f32; 4],
    pub shape2: [f32; 4],
    pub color: [f32; 4],
    pub paint: [f32; 4],
    pub pinv0: [f32; 4],
    pub pinv1: [f32; 4],
    pub gp: [f32; 4],
    pub clip: [f32; 4],
    pub rclip: [f32; 4],
    pub rclip2: [f32; 4],
}

/// The texture an instance samples (besides the atlas and the gradient ramps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TexRef {
    None,
    Image(u32),
    Transient(u32),
    Layer(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Root,
    Transient(u32),
    /// Layer id and pixel size.
    Layer(u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Batch {
    pub tex: TexRef,
    pub start: u32,
    pub end: u32,
}

pub(crate) struct PassBuild {
    pub target: Target,
    pub size: (u32, u32),
    pub instances: Vec<Instance>,
    pub batches: Vec<Batch>,
}

impl PassBuild {
    fn new() -> PassBuild {
        PassBuild { target: Target::Root, size: (0, 0), instances: Vec::new(), batches: Vec::new() }
    }
    fn reset(&mut self, target: Target, size: (u32, u32)) {
        self.target = target;
        self.size = size;
        self.instances.clear();
        self.batches.clear();
    }
    fn push(&mut self, inst: Instance, tex: TexRef) {
        let n = self.instances.len() as u32;
        self.instances.push(inst);
        if let Some(b) = self.batches.last_mut() {
            if tex == TexRef::None || b.tex == tex {
                b.end = n + 1;
                return;
            }
            if b.tex == TexRef::None {
                b.tex = tex;
                b.end = n + 1;
                return;
            }
        }
        self.batches.push(Batch { tex, start: n, end: n + 1 });
    }
}

pub(crate) const RAMP_W: usize = 256;
pub(crate) const RAMP_ROWS: usize = 256;

/// Gradient color ramps: one row of 256 texels per distinct set of stops.
pub(crate) struct Ramps {
    pub data: Vec<u8>,
    map: HashMap<u64, u32>,
    n: u32,
    pub dirty: Vec<u32>,
    pub full_dirty: bool,
}

fn hash_stops(s: &Stops) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u32| {
        h ^= v as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(s.n as u32);
    for i in 0..s.n as usize {
        mix(s.offsets[i].to_bits());
        mix(s.colors[i].r.to_bits());
        mix(s.colors[i].g.to_bits());
        mix(s.colors[i].b.to_bits());
        mix(s.colors[i].a.to_bits());
    }
    h
}

impl Ramps {
    fn new() -> Ramps {
        Ramps { data: vec![0; RAMP_W * 4 * RAMP_ROWS], map: HashMap::new(), n: 0, dirty: Vec::new(), full_dirty: true }
    }
    fn clear(&mut self) {
        self.map.clear();
        self.n = 0;
        self.full_dirty = true;
        self.dirty.clear();
    }
    /// Row for `stops`, or `None` when the table is full.
    fn row(&mut self, stops: &Stops) -> Option<u32> {
        let key = hash_stops(stops);
        if let Some(r) = self.map.get(&key) {
            return Some(*r);
        }
        if self.n as usize >= RAMP_ROWS {
            return None;
        }
        let r = self.n;
        self.n += 1;
        let base = r as usize * RAMP_W * 4;
        for x in 0..RAMP_W {
            let c = stops.color_at(x as f32 / (RAMP_W - 1) as f32);
            let o = base + x * 4;
            self.data[o] = (c.r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            self.data[o + 1] = (c.g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            self.data[o + 2] = (c.b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            self.data[o + 3] = (c.a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        self.map.insert(key, r);
        if !self.full_dirty {
            self.dirty.push(r);
        }
        Some(r)
    }
}

pub(crate) struct LayerSlot {
    pub id: u32,
    pub version: u64,
    pub size: (f32, f32),
    pub scale_bits: u32,
    pub px: (u32, u32),
    pub used: bool,
}

#[derive(Clone, Copy)]
struct ClipState {
    rect: IRect,
    /// Device rectangle and radius of a rounded clip.
    round: Option<([f32; 4], f32)>,
}

struct State {
    xf: Vec<Affine>,
    clip: ClipState,
    clips: Vec<ClipState>,
    size: (u32, u32),
}

impl State {
    fn cur(&self) -> Affine {
        *self.xf.last().unwrap()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Never,
    Opacity,
    Clip,
}

pub(crate) struct Restart;

/// Counters for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlanStats {
    pub passes: usize,
    pub instances: usize,
    pub batches: usize,
    pub atlas_masks: usize,
    pub restarts: usize,
    pub dropped: usize,
    /// Masks rasterized this frame (new atlas entries; cached masks are not counted).
    pub rasterized: usize,
    /// Largest side of any pass target this frame, in pixels.
    pub max_pass_px: u32,
}

pub(crate) struct Planner {
    pub atlas: Atlas,
    pub ramps: Ramps,
    pub layers: Vec<LayerSlot>,
    /// Finished passes, children before parents; the root pass is last.
    pub passes: Vec<PassBuild>,
    free: Vec<PassBuild>,
    rast: Rasterizer,
    flat: Flat,
    flat2: Flat,
    /// Spare room for the dashed stroke (swapped with `flat2`).
    flat3: Flat,
    stroke_scratch: StrokeScratch,
    path: Path,
    st_pool: Option<State>,
    next_transient: u32,
    restarted: bool,
    pub stats: PlanStats,
    pub transient_count: u32,
    /// Largest side of a layer texture (the device's `max_texture_dimension_2d`).
    pub max_dim: u32,
}

/// Largest texture side assumed when there is no device (the CPU-only planner, tests).
pub(crate) const MAX_DIM_DEFAULT: u32 = 16384;

impl Planner {
    pub fn new() -> Planner {
        Planner {
            atlas: Atlas::new(),
            ramps: Ramps::new(),
            layers: Vec::new(),
            passes: Vec::new(),
            free: Vec::new(),
            rast: Rasterizer::new(),
            flat: Flat::new(),
            flat2: Flat::new(),
            flat3: Flat::new(),
            stroke_scratch: StrokeScratch::default(),
            path: Path::new(),
            st_pool: None,
            next_transient: 0,
            restarted: false,
            stats: PlanStats::default(),
            transient_count: 0,
            max_dim: MAX_DIM_DEFAULT,
        }
    }

    /// Return the finished passes to the pool (call after the frame was executed).
    pub fn recycle(&mut self) {
        self.free.append(&mut self.passes);
    }

    fn new_pass(&mut self, target: Target, size: (u32, u32)) -> PassBuild {
        let mut p = self.free.pop().unwrap_or_else(PassBuild::new);
        p.reset(target, size);
        p
    }

    /// Build the passes for one frame.
    pub fn plan(&mut self, scene: &Scene, images: &ImageStore, opts: &RenderOptions) {
        self.recycle();
        self.restarted = false;
        self.stats = PlanStats::default();
        loop {
            self.atlas.begin_frame();
            self.next_transient = 0;
            for l in &mut self.layers {
                l.used = false;
            }
            let mut st = self.st_pool.take().unwrap_or_else(|| State {
                xf: Vec::new(),
                clip: ClipState { rect: IRect::new(0, 0, 0, 0), round: None },
                clips: Vec::new(),
                size: (0, 0),
            });
            st.xf.clear();
            st.xf.push(Affine::scale(opts.scale, opts.scale));
            st.clips.clear();
            st.size = opts.size_px;
            st.clip = ClipState { rect: IRect::new(0, 0, opts.size_px.0 as i32, opts.size_px.1 as i32), round: None };
            let mut root = self.new_pass(Target::Root, opts.size_px);
            let mut pos = 0usize;
            let r = self.exec(scene, images, &mut pos, &mut root, &mut st, Stop::Never);
            self.st_pool = Some(st);
            match r {
                Ok(()) => {
                    self.passes.push(root);
                    break;
                }
                Err(Restart) => {
                    // Atlas or gradient table full: start over with everything cleared.
                    self.free.push(root);
                    let done: Vec<PassBuild> = std::mem::take(&mut self.passes);
                    self.free.extend(done);
                    self.atlas.clear();
                    self.ramps.clear();
                    self.layers.clear();
                    self.restarted = true;
                    self.stats.restarts += 1;
                }
            }
        }
        self.layers.retain(|l| l.used);
        self.transient_count = self.next_transient;
        self.stats.passes = self.passes.len();
        self.stats.instances = self.passes.iter().map(|p| p.instances.len()).sum();
        self.stats.batches = self.passes.iter().map(|p| p.batches.len()).sum();
        self.stats.atlas_masks = self.atlas.len();
        self.stats.max_pass_px = self.passes.iter().map(|p| p.size.0.max(p.size.1)).max().unwrap_or(0);
    }

    // ---- helpers ---------------------------------------------------------------------------

    fn emit(&mut self, pb: &mut PassBuild, st: &State, mut inst: Instance, tex: TexRef, dev: Rect) {
        let c = st.clip.rect;
        let x0 = dev.x.max(c.x0 as f32);
        let y0 = dev.y.max(c.y0 as f32);
        let x1 = dev.right().min(c.x1 as f32);
        let y1 = dev.bottom().min(c.y1 as f32);
        if !(x1 > x0 && y1 > y0) {
            return;
        }
        inst.bbox = [x0, y0, x1, y1];
        inst.clip = [c.x0 as f32, c.y0 as f32, c.x1 as f32, c.y1 as f32];
        match st.clip.round {
            Some((r, rad)) => {
                inst.rclip = r;
                inst.rclip2 = [rad, 0.0, 0.0, 0.0];
            }
            None => inst.rclip2 = [-1.0, 0.0, 0.0, 0.0],
        }
        pb.push(inst, tex);
    }

    /// Fill the paint fields of `inst`. `Ok(None)` means the paint cannot be drawn (missing
    /// image, degenerate transform): skip the primitive.
    fn apply_paint(&mut self, images: &ImageStore, paint: &Paint, xf: &Affine, inst: &mut Instance) -> Result<Option<TexRef>, Restart> {
        match paint {
            Paint::Solid(c) => {
                inst.paint = [PAINT_SOLID, 0.0, 0.0, 0.0];
                inst.color = [c.r, c.g, c.b, c.a];
                Ok(Some(TexRef::None))
            }
            Paint::Linear { start, end, stops } => {
                let d = *end - *start;
                let l2 = d.dot(d);
                let Some(inv) = xf.invert() else { return Ok(None) };
                let Some(row) = self.ramp_row(stops)? else { return Ok(None) };
                inst.paint = [PAINT_LINEAR, row as f32, 0.0, if l2 > 1e-12 { 1.0 / l2 } else { 0.0 }];
                inst.pinv0 = [inv.0[0], inv.0[1], inv.0[2], inv.0[3]];
                inst.pinv1 = [inv.0[4], inv.0[5], 0.0, 0.0];
                inst.gp = [start.x, start.y, d.x, d.y];
                inst.color = [1.0; 4];
                Ok(Some(TexRef::None))
            }
            Paint::Radial { center, radius, stops } => {
                let Some(inv) = xf.invert() else { return Ok(None) };
                let Some(row) = self.ramp_row(stops)? else { return Ok(None) };
                inst.paint = [PAINT_RADIAL, row as f32, 0.0, 0.0];
                inst.pinv0 = [inv.0[0], inv.0[1], inv.0[2], inv.0[3]];
                inst.pinv1 = [inv.0[4], inv.0[5], 0.0, 0.0];
                inst.gp = [center.x, center.y, radius.max(1e-6), 0.0];
                inst.color = [1.0; 4];
                Ok(Some(TexRef::None))
            }
            Paint::Image { image, transform, sampling } => {
                let Some(img) = images.get(*image) else { return Ok(None) };
                let Some(inv) = transform.then(xf).invert() else { return Ok(None) };
                inst.paint = [PAINT_IMAGE, 0.0, if *sampling == Sampling::Nearest { 1.0 } else { 0.0 }, 0.0];
                inst.pinv0 = [inv.0[0], inv.0[1], inv.0[2], inv.0[3]];
                inst.pinv1 = [inv.0[4], inv.0[5], img.width as f32, img.height as f32];
                inst.color = [1.0; 4];
                Ok(Some(TexRef::Image(image.0)))
            }
        }
    }

    /// Gradient row; `Ok(None)` when the table stays full after a restart (the gradient is
    /// dropped).
    fn ramp_row(&mut self, stops: &Stops) -> Result<Option<u32>, Restart> {
        match self.ramps.row(stops) {
            Some(r) => Ok(Some(r)),
            None if self.restarted => {
                self.stats.dropped += 1;
                Ok(None)
            }
            None => Err(Restart),
        }
    }

    /// An instance with the geometry transform and shape set.
    fn shape_instance(kind: f32, xf: &Affine, shape: [f32; 4], shape2: [f32; 4]) -> Option<Instance> {
        let inv = xf.invert()?;
        let mut i = Instance::default();
        i.inv0 = [inv.0[0], inv.0[1], inv.0[2], inv.0[3]];
        i.inv1 = [inv.0[4], inv.0[5], xf.mean_scale(), kind];
        i.shape = shape;
        i.shape2 = shape2;
        Some(i)
    }

    /// Instance for a cached/rasterized mask at device position `(dx, dy)`.
    fn mask_instance(entry: &crate::atlas::Entry, dx: i32, dy: i32) -> (Instance, Rect) {
        let mut i = Instance::default();
        i.inv0 = [1.0, 0.0, 0.0, 1.0];
        i.inv1 = [0.0, 0.0, 1.0, KIND_MASK];
        i.shape = [entry.x as f32, entry.y as f32, entry.w as f32, entry.h as f32];
        i.shape2 = [dx as f32, dy as f32, 0.0, 0.0];
        (i, Rect::new(dx as f32, dy as f32, entry.w as f32, entry.h as f32))
    }

    /// Rasterize `self.flat` (if not cached under `key`) and return its atlas entry.
    /// The mask of `self.flat` (flattened with `shape.xf`), from the atlas or rasterized now.
    /// Its offset is relative to the frame without the whole translation: add `shape.off`.
    fn mask_for_flat(&mut self, shape: &Shape, rule: FillRule, size: (u32, u32)) -> Result<Option<crate::atlas::Entry>, Restart> {
        if let Some(e) = self.atlas.get(&shape.free) {
            return Ok(Some(e));
        }
        let bb = self.flat.bounds();
        let fits = bb.x.is_finite() && bb.y.is_finite() && bb.w.is_finite() && bb.h.is_finite() && bb.x.abs() < SPLIT_MAX && bb.y.abs() < SPLIT_MAX && bb.w <= FREE_MAX && bb.h <= FREE_MAX;
        if fits {
            let mask = self.rast.fill(&self.flat, rule, IRect::outset(&bb));
            return self.insert_mask(shape.free, &mask, mask.x, mask.y);
        }
        // too big for the atlas: the part inside the target, as before
        if let Some(e) = self.atlas.get(&shape.fixed) {
            return Ok(Some(e));
        }
        let (ox, oy) = shape.off;
        let clip = IRect::new(-ox, -oy, size.0 as i32 - ox, size.1 as i32 - oy);
        let mask = self.rast.fill(&self.flat, rule, clip);
        self.insert_mask(shape.fixed, &mask, mask.x, mask.y)
    }

    fn insert_mask(&mut self, key: MaskKey, mask: &crate::raster::Mask, ox: i32, oy: i32) -> Result<Option<crate::atlas::Entry>, Restart> {
        self.stats.rasterized += 1;
        match self.atlas.insert(key, mask, ox, oy) {
            Ok(e) => Ok(Some(e)),
            Err(InsertError::Full) => {
                if self.restarted {
                    self.stats.dropped += 1;
                    Ok(None)
                } else {
                    Err(Restart)
                }
            }
        }
    }

    fn xf_bits(xf: &Affine) -> [u32; 6] {
        let m = xf.0;
        [m[0].to_bits(), m[1].to_bits(), m[2].to_bits(), m[3].to_bits(), m[4].to_bits(), m[5].to_bits()]
    }

    fn fill_flat_op(&mut self, pb: &mut PassBuild, st: &State, images: &ImageStore, shape: &Shape, rule: FillRule, paint: &Paint, xf: &Affine) -> Result<(), Restart> {
        let Some(entry) = self.mask_for_flat(shape, rule, st.size)? else { return Ok(()) };
        if entry.w == 0 {
            return Ok(());
        }
        let (mut inst, dev) = Self::mask_instance(&entry, entry.mx + shape.off.0, entry.my + shape.off.1);
        let Some(tex) = self.apply_paint(images, paint, xf, &mut inst)? else { return Ok(()) };
        self.emit(pb, st, inst, tex, dev);
        Ok(())
    }

    // ---- the walk --------------------------------------------------------------------------

    fn exec(&mut self, scene: &Scene, images: &ImageStore, pos: &mut usize, pb: &mut PassBuild, st: &mut State, stop: Stop) -> Result<(), Restart> {
        let xf_base = st.xf.len();
        let clip_base = st.clips.len();
        while *pos < scene.ops.len() {
            let op = scene.ops[*pos];
            *pos += 1;
            match op {
                Op::Rect { rect, paint } => self.prim_rrect(pb, st, images, rect, 0.0, &scene.paints[paint as usize])?,
                Op::RoundRect { rect, radius, paint } => self.prim_rrect(pb, st, images, rect, radius, &scene.paints[paint as usize])?,
                Op::Ellipse { center, rx, ry, paint } => {
                    let xf = st.cur();
                    let aa = 1.5 / xf.mean_scale().max(1e-6);
                    let Some(mut inst) = Self::shape_instance(KIND_ELLIPSE, &xf, [center.x, center.y, rx, ry], [0.0; 4]) else { continue };
                    let dev = xf.map_rect(&Rect::new(center.x - rx, center.y - ry, rx * 2.0, ry * 2.0).inflate(aa));
                    let Some(tex) = self.apply_paint(images, &scene.paints[paint as usize], &xf, &mut inst)? else { continue };
                    self.emit(pb, st, inst, tex, dev);
                }
                Op::Line { a, b, width, cap, paint } => {
                    self.prim_segment(pb, st, images, a, b, width, cap, cap, &scene.paints[paint as usize])?;
                }
                Op::Polyline { p0, p1, width, cap, paint } => {
                    let pts = &scene.pts[p0 as usize..p1 as usize];
                    let n = pts.len();
                    let paint = &scene.paints[paint as usize];
                    for i in 0..n - 1 {
                        let c0 = if i == 0 { cap } else { Cap::Butt };
                        let c1 = if i + 2 == n { cap } else { Cap::Butt };
                        self.prim_segment(pb, st, images, pts[i], pts[i + 1], width, c0, c1, paint)?;
                    }
                    // round joins
                    for p in &pts[1..n - 1] {
                        let xf = st.cur();
                        let hw = width * 0.5;
                        let aa = 1.5 / xf.mean_scale().max(1e-6);
                        let Some(mut inst) = Self::shape_instance(KIND_ELLIPSE, &xf, [p.x, p.y, hw, hw], [0.0; 4]) else { continue };
                        let dev = xf.map_rect(&Rect::new(p.x - hw, p.y - hw, width, width).inflate(aa));
                        let Some(tex) = self.apply_paint(images, paint, &xf, &mut inst)? else { continue };
                        self.emit(pb, st, inst, tex, dev);
                    }
                }
                Op::FillPath { path, rule, paint } => {
                    let xf = st.cur();
                    let shape = Shape::new(&xf, 1, path.hash, rule as u64, (rule as u64) | ((st.size.0 as u64) << 8) | ((st.size.1 as u64) << 32));
                    if self.atlas.get(&shape.free).is_none() {
                        flatten_raw(&scene.verbs[path.v0 as usize..path.v1 as usize], &scene.pts[path.p0 as usize..path.p1 as usize], &shape.xf, TOL, &mut self.flat);
                    }
                    self.fill_flat_op(pb, st, images, &shape, rule, &scene.paints[paint as usize], &xf)?;
                }
                Op::StrokePath { path, stroke, paint } => {
                    let xf = st.cur();
                    let stroke = scene.strokes[stroke as usize];
                    let sh = stroke_hash(&stroke);
                    let shape = Shape::new(&xf, 2, path.hash, sh, sh ^ ((st.size.0 as u64) << 20) ^ ((st.size.1 as u64) << 40));
                    if self.atlas.get(&shape.free).is_none() {
                        let tol_local = TOL / xf.max_scale().max(1e-6);
                        flatten_raw(
                            &scene.verbs[path.v0 as usize..path.v1 as usize],
                            &scene.pts[path.p0 as usize..path.p1 as usize],
                            &Affine::IDENTITY,
                            tol_local,
                            &mut self.flat2,
                        );
                        if let Some(d) = stroke.dash {
                            std::mem::swap(&mut self.flat2, &mut self.flat3);
                            dash_flat(&self.flat3, &d, &mut self.flat2);
                        }
                        stroke_polys_with(&self.flat2, &stroke, &shape.xf, tol_local, &mut self.flat, &mut self.stroke_scratch);
                    }
                    self.fill_flat_op(pb, st, images, &shape, FillRule::NonZero, &scene.paints[paint as usize], &xf)?;
                }
                Op::Shadow { rect, radius, style } => {
                    let xf = st.cur();
                    let sigma = style.blur * 0.5;
                    let shifted = rect.translate(style.offset);
                    let reach = 3.0 * sigma.max(0.0) + 1.5 / xf.mean_scale().max(1e-6);
                    let Some(mut inst) = Self::shape_instance(KIND_SHADOW, &xf, [shifted.x, shifted.y, shifted.w, shifted.h], [radius, sigma, 0.0, 0.0]) else { continue };
                    inst.paint = [PAINT_SOLID, 0.0, 0.0, 0.0];
                    inst.color = [style.color.r, style.color.g, style.color.b, style.color.a];
                    let dev = xf.map_rect(&shifted.inflate(reach));
                    self.emit(pb, st, inst, TexRef::None, dev);
                }
                Op::Image { image, dst, sampling } => {
                    let Some(img) = images.get(image) else { continue };
                    let t = Affine::scale(dst.w / img.width as f32, dst.h / img.height as f32).then(&Affine::translate(dst.x, dst.y));
                    let paint = Paint::Image { image, transform: t, sampling };
                    self.prim_rrect(pb, st, images, dst, 0.0, &paint)?;
                }
                Op::Glyphs { run, paint } => self.prim_glyphs(pb, st, images, scene, run as usize, &scene.paints[paint as usize])?,
                Op::PushClipRect(r) => {
                    let xf = st.cur();
                    if xf.is_scale_translate() {
                        let b = IRect::round(&xf.map_rect(&r));
                        st.clips.push(st.clip);
                        st.clip.rect = st.clip.rect.intersect(&b);
                    } else {
                        self.path.clear();
                        self.path.rect(r);
                        self.path_clip_group(scene, images, pos, pb, st, FillRule::NonZero, 3, 0)?;
                    }
                }
                Op::PushClipRoundRect { rect, radius } => {
                    let xf = st.cur();
                    let uniform = xf.is_scale_translate() && (xf.0[0] - xf.0[3]).abs() < 1e-4 * xf.0[0].abs().max(1.0) && xf.0[0] > 0.0;
                    if uniform {
                        let dr = xf.map_rect(&rect);
                        st.clips.push(st.clip);
                        let b = IRect::outset(&dr);
                        if let Some((old, _)) = st.clip.round {
                            // second rounded clip: the outer one keeps only its rectangle
                            let o = IRect::outset(&Rect::from_ltrb(old[0], old[1], old[2], old[3]));
                            st.clip.rect = st.clip.rect.intersect(&o);
                        }
                        st.clip.rect = st.clip.rect.intersect(&b);
                        st.clip.round = Some(([dr.x, dr.y, dr.right(), dr.bottom()], radius * xf.0[0]));
                    } else {
                        self.path.clear();
                        self.path.round_rect(rect, radius);
                        self.path_clip_group(scene, images, pos, pb, st, FillRule::NonZero, 4, 0)?;
                    }
                }
                Op::PushClipPath { path, rule } => {
                    self.path.clear();
                    self.path.verbs.extend_from_slice(&scene.verbs[path.v0 as usize..path.v1 as usize]);
                    self.path.pts.extend_from_slice(&scene.pts[path.p0 as usize..path.p1 as usize]);
                    self.path_clip_group(scene, images, pos, pb, st, rule, 5, path.hash)?;
                }
                Op::PopClip => {
                    if st.clips.len() > clip_base {
                        st.clip = st.clips.pop().unwrap();
                    } else if stop == Stop::Clip {
                        // restore the transform stack to what the group started with
                        st.xf.truncate(xf_base.max(1));
                        return Ok(());
                    }
                }
                Op::PushTransform(t) => {
                    let n = t.then(&st.cur());
                    st.xf.push(n);
                }
                Op::PopTransform => {
                    if st.xf.len() > xf_base.max(1) {
                        st.xf.pop();
                    }
                }
                Op::PushOpacity(o) => {
                    let slot = self.next_transient;
                    self.next_transient += 1;
                    let mut group = self.new_pass(Target::Transient(slot), pb.size);
                    let saved = st.clip;
                    let saved_stack_len = st.clips.len();
                    st.clip = ClipState { rect: IRect::new(0, 0, pb.size.0 as i32, pb.size.1 as i32), round: None };
                    self.exec(scene, images, pos, &mut group, st, Stop::Opacity)?;
                    st.clips.truncate(saved_stack_len);
                    st.clip = saved;
                    self.passes.push(group);
                    let mut inst = Instance::default();
                    inst.inv0 = [1.0, 0.0, 0.0, 1.0];
                    inst.inv1 = [0.0, 0.0, 1.0, KIND_FULL];
                    inst.paint = [PAINT_IMAGE, 0.0, 1.0, 0.0];
                    inst.pinv0 = [1.0, 0.0, 0.0, 1.0];
                    inst.pinv1 = [0.0, 0.0, pb.size.0 as f32, pb.size.1 as f32];
                    inst.color = [1.0, 1.0, 1.0, o];
                    let dev = Rect::new(0.0, 0.0, pb.size.0 as f32, pb.size.1 as f32);
                    self.emit(pb, st, inst, TexRef::Transient(slot), dev);
                }
                Op::PopOpacity => {
                    if stop == Stop::Opacity {
                        st.xf.truncate(xf_base.max(1));
                        return Ok(());
                    }
                }
                Op::Layer(idx) => self.prim_layer(pb, st, images, scene, &scene.layers[idx as usize])?,
            }
        }
        Ok(())
    }

    fn prim_rrect(&mut self, pb: &mut PassBuild, st: &State, images: &ImageStore, rect: Rect, radius: f32, paint: &Paint) -> Result<(), Restart> {
        let xf = st.cur();
        let aa = 1.5 / xf.mean_scale().max(1e-6);
        let Some(mut inst) = Self::shape_instance(KIND_RRECT, &xf, [rect.x, rect.y, rect.w, rect.h], [radius, 0.0, 0.0, 0.0]) else { return Ok(()) };
        let dev = xf.map_rect(&rect.inflate(aa));
        let Some(tex) = self.apply_paint(images, paint, &xf, &mut inst)? else { return Ok(()) };
        self.emit(pb, st, inst, tex, dev);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn prim_segment(&mut self, pb: &mut PassBuild, st: &State, images: &ImageStore, a: Point, b: Point, width: f32, cap0: Cap, cap1: Cap, paint: &Paint) -> Result<(), Restart> {
        let d = b - a;
        let len = d.length();
        if len < 1e-6 && cap0 == Cap::Butt {
            return Ok(());
        }
        let dir = if len > 1e-6 { d * (1.0 / len) } else { Point::new(1.0, 0.0) };
        let hw = width * 0.5;
        let mid = a.lerp(b, 0.5);
        let xf = st.cur();
        let capf = |c: Cap| match c {
            Cap::Butt => 0.0,
            Cap::Round => 1.0,
            Cap::Square => 2.0,
        };
        let Some(mut inst) = Self::shape_instance(KIND_LINE, &xf, [mid.x, mid.y, len * 0.5, hw], [dir.x, dir.y, capf(cap0), capf(cap1)]) else { return Ok(()) };
        let aa = 1.5 / xf.mean_scale().max(1e-6);
        let ext = hw * 1.5 + aa;
        let local = Rect::from_ltrb(a.x.min(b.x) - ext, a.y.min(b.y) - ext, a.x.max(b.x) + ext, a.y.max(b.y) + ext);
        let dev = xf.map_rect(&local);
        let Some(tex) = self.apply_paint(images, paint, &xf, &mut inst)? else { return Ok(()) };
        self.emit(pb, st, inst, tex, dev);
        Ok(())
    }

    fn prim_glyphs(&mut self, pb: &mut PassBuild, st: &State, images: &ImageStore, scene: &Scene, run: usize, paint: &Paint) -> Result<(), Restart> {
        let r = &scene.runs[run];
        let xf = st.cur();
        let upright = xf.is_scale_translate() && xf.0[0] > 0.0 && xf.0[3] > 0.0 && (xf.0[0] - xf.0[3]).abs() < 1e-4;
        let mut template = Instance::default();
        template.inv0 = [1.0, 0.0, 0.0, 1.0];
        template.inv1 = [0.0, 0.0, 1.0, KIND_MASK];
        let Some(tex) = self.apply_paint(images, paint, &xf, &mut template)? else { return Ok(()) };
        for g in &scene.glyphs[r.g0 as usize..r.g1 as usize] {
            if upright {
                let dev = xf.apply(Point::new(g.x, g.y));
                let size_px = r.size * xf.0[0];
                let (ix, iy, sub) = glyph::place(dev);
                let key = GlyphKey { font: r.font.uid(), glyph: g.glyph, size: glyph::quantize_size(size_px), sub };
                let mkey = MaskKey::glyph(key.font, key.glyph, key.size, key.sub);
                let entry = match self.atlas.get(&mkey) {
                    Some(e) => e,
                    None => {
                        let face = r.font.face();
                        let mask = glyph::rasterize(&r.font, &face, &key, &mut self.rast, &mut self.path, &mut self.flat);
                        match self.insert_mask(mkey, &mask, mask.x, mask.y)? {
                            Some(e) => e,
                            None => continue,
                        }
                    }
                };
                if entry.w == 0 {
                    continue;
                }
                let (mut inst, devr) = Self::mask_instance(&entry, ix + entry.mx, iy + entry.my);
                inst.paint = template.paint;
                inst.color = template.color;
                inst.pinv0 = template.pinv0;
                inst.pinv1 = template.pinv1;
                inst.gp = template.gp;
                self.emit(pb, st, inst, tex, devr);
            } else {
                let gx = Affine::translate(g.x, g.y).then(&xf);
                let gid = (g.glyph as u64) << 32;
                let shape = Shape::new(&gx, 3, r.font.uid(), gid | r.size.to_bits() as u64, gid | r.size.to_bits() as u64 ^ ((st.size.0 as u64) << 48));
                if self.atlas.get(&shape.free).is_none() {
                    let face = r.font.face();
                    self.path.clear();
                    if !r.font.outline(&face, g.glyph, r.size, &mut self.path) {
                        continue;
                    }
                    self.path.flatten(&shape.xf, TOL, &mut self.flat);
                }
                let Some(entry) = self.mask_for_flat(&shape, FillRule::NonZero, st.size)? else { continue };
                if entry.w == 0 {
                    continue;
                }
                let (mut inst, devr) = Self::mask_instance(&entry, entry.mx + shape.off.0, entry.my + shape.off.1);
                inst.paint = template.paint;
                inst.color = template.color;
                inst.pinv0 = template.pinv0;
                inst.pinv1 = template.pinv1;
                inst.gp = template.gp;
                self.emit(pb, st, inst, tex, devr);
            }
        }
        Ok(())
    }

    /// Everything until the matching `PopClip` is drawn into a texture, then composited through
    /// the clip path's coverage mask. The path to clip with is in `self.path` (local space).
    #[allow(clippy::too_many_arguments)]
    fn path_clip_group(&mut self, scene: &Scene, images: &ImageStore, pos: &mut usize, pb: &mut PassBuild, st: &mut State, rule: FillRule, kind: u8, hash: u64) -> Result<(), Restart> {
        let xf = st.cur();
        let mut h = hash;
        if kind != 5 {
            for p in &self.path.pts {
                h = h.wrapping_mul(0x0000_0100_0000_01b3) ^ p.x.to_bits() as u64 ^ ((p.y.to_bits() as u64) << 21);
            }
        }
        let shape = Shape::new(&xf, 8 + kind, h, rule as u64, (rule as u64) | ((st.size.0 as u64) << 8) | ((st.size.1 as u64) << 32));
        if self.atlas.get(&shape.free).is_none() {
            let path = std::mem::take(&mut self.path);
            path.flatten(&shape.xf, TOL, &mut self.flat);
            self.path = path;
        }
        let entry = self.mask_for_flat(&shape, rule, st.size)?;
        let slot = self.next_transient;
        self.next_transient += 1;
        let mut group = self.new_pass(Target::Transient(slot), pb.size);
        let saved = st.clip;
        let saved_len = st.clips.len();
        st.clip = ClipState { rect: IRect::new(0, 0, pb.size.0 as i32, pb.size.1 as i32), round: None };
        let xf_len = st.xf.len();
        self.exec(scene, images, pos, &mut group, st, Stop::Clip)?;
        st.xf.truncate(xf_len);
        st.clips.truncate(saved_len);
        st.clip = saved;
        self.passes.push(group);
        let Some(entry) = entry else { return Ok(()) };
        if entry.w == 0 {
            return Ok(());
        }
        let (mut inst, dev) = Self::mask_instance(&entry, entry.mx + shape.off.0, entry.my + shape.off.1);
        inst.paint = [PAINT_IMAGE, 0.0, 1.0, 0.0];
        inst.pinv0 = [1.0, 0.0, 0.0, 1.0];
        inst.pinv1 = [0.0, 0.0, pb.size.0 as f32, pb.size.1 as f32];
        inst.color = [1.0; 4];
        self.emit(pb, st, inst, TexRef::Transient(slot), dev);
        Ok(())
    }

    fn prim_layer(&mut self, pb: &mut PassBuild, st: &mut State, images: &ImageStore, scene: &Scene, entry: &LayerEntry) -> Result<(), Restart> {
        // scale of the layer texture: the transform it is placed with (see `layer_scale`)
        // The texture must fit the device's largest texture. A smaller one is stretched back over
        // the layer (`pinv` below uses the same scale), so the layer is blurred, not dropped.
        let longest = entry.size.0.max(entry.size.1).max(1.0);
        let scale = crate::scene::layer_scale(&st.cur(), st.xf[0].0[0], entry.size).min(self.max_dim as f32 / longest);
        let pw = (entry.size.0 * scale).ceil().max(1.0) as u32;
        let ph = (entry.size.1 * scale).ceil().max(1.0) as u32;
        let sbits = scale.to_bits();
        let slot = self.layers.iter().position(|l| l.id == entry.id.0);
        let fresh = match slot {
            Some(i) => {
                let l = &self.layers[i];
                l.version != entry.version || l.size != entry.size || l.scale_bits != sbits || l.px != (pw, ph)
            }
            None => true,
        };
        let idx = match slot {
            Some(i) => i,
            None => {
                self.layers.push(LayerSlot { id: entry.id.0, version: 0, size: (-1.0, -1.0), scale_bits: 0, px: (0, 0), used: false });
                self.layers.len() - 1
            }
        };
        self.layers[idx].used = true;
        if fresh {
            let mut lst = State {
                xf: vec![Affine::scale(scale, scale)],
                clip: ClipState { rect: IRect::new(0, 0, pw as i32, ph as i32), round: None },
                clips: Vec::new(),
                size: (pw, ph),
            };
            let mut lpb = self.new_pass(Target::Layer(entry.id.0), (pw, ph));
            let mut pos = 0usize;
            self.exec(&entry.scene, images, &mut pos, &mut lpb, &mut lst, Stop::Never)?;
            self.passes.push(lpb);
            let l = &mut self.layers[idx];
            l.version = entry.version;
            l.size = entry.size;
            l.scale_bits = sbits;
            l.px = (pw, ph);
        }
        let _ = scene;
        let xf = st.cur();
        let Some(inv) = xf.invert() else { return Ok(()) };
        let r = Rect::new(0.0, 0.0, entry.size.0, entry.size.1);
        let aa = 1.5 / xf.mean_scale().max(1e-6);
        let Some(mut inst) = Self::shape_instance(KIND_RRECT, &xf, [0.0, 0.0, entry.size.0, entry.size.1], [0.0; 4]) else { return Ok(()) };
        let pinv = inv.then(&Affine::scale(scale, scale));
        inst.paint = [PAINT_IMAGE, 0.0, 0.0, 0.0];
        inst.pinv0 = [pinv.0[0], pinv.0[1], pinv.0[2], pinv.0[3]];
        inst.pinv1 = [pinv.0[4], pinv.0[5], pw as f32, ph as f32];
        inst.color = [1.0; 4];
        let dev = xf.map_rect(&r.inflate(aa));
        self.emit(pb, st, inst, TexRef::Layer(entry.id.0), dev);
        Ok(())
    }
}

fn stroke_hash(s: &Stroke) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u32| {
        h ^= v as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(s.width.to_bits());
    mix(match s.join {
        Join::Miter => 0,
        Join::Round => 1,
        Join::Bevel => 2,
    });
    mix(match s.cap {
        Cap::Butt => 0,
        Cap::Round => 1,
        Cap::Square => 2,
    });
    mix(s.miter_limit.to_bits());
    if let Some(d) = s.dash {
        mix(d.len as u32 + 100);
        for v in d.slice() {
            mix(v.to_bits());
        }
        mix(d.phase.to_bits());
    }
    h
}
