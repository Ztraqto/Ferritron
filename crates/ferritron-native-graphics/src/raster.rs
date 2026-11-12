//! CPU coverage computation: exact-area scanline rasterization of polygons into an 8-bit mask.
//!
//! This is the one rule both backends follow for paths and glyphs: coverage is the exact
//! area of the pixel inside the shape (signed-area accumulation), with the fill rule applied
//! to the accumulated winding.

use crate::geom::{IRect, Point};
use crate::path::{FillRule, Flat};

/// An 8-bit coverage mask positioned in device pixels.
#[derive(Clone, Debug, Default)]
pub struct Mask {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
    pub fn bounds(&self) -> IRect {
        IRect::new(self.x, self.y, self.x + self.w as i32, self.y + self.h as i32)
    }
    /// Coverage at a device pixel (0 outside).
    pub fn get(&self, px: i32, py: i32) -> u8 {
        let lx = px - self.x;
        let ly = py - self.y;
        if lx < 0 || ly < 0 || lx >= self.w as i32 || ly >= self.h as i32 {
            0
        } else {
            self.data[(ly as u32 * self.w + lx as u32) as usize]
        }
    }
}

/// Reusable rasterizer; keeps its accumulation buffer between calls.
#[derive(Default)]
pub struct Rasterizer {
    acc: Vec<f32>,
    stride: usize,
    w: usize,
    h: usize,
}

impl Rasterizer {
    pub fn new() -> Rasterizer {
        Rasterizer::default()
    }

    /// Rasterize all sub-paths of `flat` (device-space points, implicitly closed) restricted
    /// to `clip`. Returns an empty mask when nothing is covered.
    pub fn fill(&mut self, flat: &Flat, rule: FillRule, clip: IRect) -> Mask {
        let mut mask = Mask::default();
        self.fill_into(flat, rule, clip, &mut mask);
        mask
    }

    /// Like [`Rasterizer::fill`], but writes into `out` and reuses its buffer (no allocation
    /// once `out` has held a mask of at least this size).
    pub fn fill_into(&mut self, flat: &Flat, rule: FillRule, clip: IRect, out: &mut Mask) {
        out.x = 0;
        out.y = 0;
        out.w = 0;
        out.h = 0;
        out.data.clear();
        let b = flat.bounds();
        if flat.pts.is_empty() || !b.x.is_finite() || !b.y.is_finite() || !b.right().is_finite() || !b.bottom().is_finite() {
            return;
        }
        let area = IRect::outset(&b).intersect(&clip);
        if area.is_empty() {
            return;
        }
        self.reset(area.width() as usize, area.height() as usize);
        let ox = area.x0 as f32;
        let oy = area.y0 as f32;
        for s in &flat.subs {
            let pts = flat.sub_points(s);
            if pts.len() < 2 {
                continue;
            }
            for i in 0..pts.len() {
                let a = pts[i];
                let c = pts[(i + 1) % pts.len()];
                self.line(Point::new(a.x - ox, a.y - oy), Point::new(c.x - ox, c.y - oy));
            }
        }
        out.x = area.x0;
        out.y = area.y0;
        out.w = self.w as u32;
        out.h = self.h as u32;
        out.data.resize(self.w * self.h, 0);
        self.resolve(rule, &mut out.data);
    }

    fn reset(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.stride = w + 2;
        let n = self.stride * h;
        self.acc.clear();
        self.acc.resize(n, 0.0);
    }

    fn resolve(&self, rule: FillRule, out: &mut [u8]) {
        for y in 0..self.h {
            let row = &self.acc[y * self.stride..y * self.stride + self.w];
            let orow = &mut out[y * self.w..(y + 1) * self.w];
            let mut sum = 0.0f32;
            for (o, a) in orow.iter_mut().zip(row) {
                sum += *a;
                let c = match rule {
                    FillRule::NonZero => sum.abs().min(1.0),
                    FillRule::EvenOdd => {
                        let t = sum.abs().rem_euclid(2.0);
                        if t > 1.0 { 2.0 - t } else { t }
                    }
                };
                *o = (c * 255.0 + 0.5) as u8;
            }
        }
    }

    /// Add an edge, clipping to the buffer: parts left of x = 0 collapse onto column 0 (they
    /// still change the winding of everything to their right), parts right of x = w vanish.
    fn line(&mut self, p0: Point, p1: Point) {
        if p0.y == p1.y {
            return;
        }
        let w = self.w as f32;
        let (mut a, mut b) = (p0, p1);
        // split at x = 0 and x = w
        let mut pieces: [(Point, Point); 3] = [(a, a); 3];
        let mut np = 0;
        let dir_x = b.x - a.x;
        let mut cuts = [0.0f32; 2];
        let mut nc = 0;
        if dir_x != 0.0 {
            for edge in [0.0f32, w] {
                let t = (edge - a.x) / dir_x;
                if t > 0.0 && t < 1.0 {
                    cuts[nc] = t;
                    nc += 1;
                }
            }
            if nc == 2 && cuts[0] > cuts[1] {
                cuts.swap(0, 1);
            }
        }
        let mut t0 = 0.0f32;
        let (sa, sb) = (a, b);
        for i in 0..=nc {
            let t1 = if i < nc { cuts[i] } else { 1.0 };
            a = sa.lerp(sb, t0);
            b = sa.lerp(sb, t1);
            pieces[np] = (a, b);
            np += 1;
            t0 = t1;
        }
        for (a, b) in pieces.iter().take(np) {
            let mid = (a.x + b.x) * 0.5;
            let (mut qa, mut qb) = (*a, *b);
            if mid <= 0.0 {
                qa.x = 0.0;
                qb.x = 0.0;
            } else if mid >= w {
                qa.x = w;
                qb.x = w;
            } else {
                qa.x = qa.x.clamp(0.0, w);
                qb.x = qb.x.clamp(0.0, w);
            }
            self.draw_line(qa, qb);
        }
    }

    fn draw_line(&mut self, p0: Point, p1: Point) {
        if p0.y == p1.y {
            return;
        }
        let (dir, p0, p1) = if p0.y < p1.y { (1.0f32, p0, p1) } else { (-1.0f32, p1, p0) };
        let dxdy = (p1.x - p0.x) / (p1.y - p0.y);
        let mut x = p0.x;
        let y0 = if p0.y < 0.0 { 0 } else { p0.y as usize };
        if p0.y < 0.0 {
            x -= p0.y * dxdy;
        }
        let ymax = (p1.y.ceil().max(0.0) as usize).min(self.h);
        for y in y0..ymax {
            let linestart = y * self.stride;
            let dy = ((y + 1) as f32).min(p1.y) - (y as f32).max(p0.y);
            let wf = self.w as f32;
            x = x.clamp(0.0, wf);
            let xnext = (x + dxdy * dy).clamp(0.0, wf);
            let d = dy * dir;
            let (x0, x1) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let x0floor = x0.floor();
            let x0i = x0floor as i32;
            let x1ceil = x1.ceil();
            let x1i = x1ceil as i32;
            if x1i <= x0i + 1 {
                let xmf = 0.5 * (x + xnext) - x0floor;
                self.acc[linestart + x0i as usize] += d - d * xmf;
                self.acc[linestart + (x0i + 1) as usize] += d * xmf;
            } else {
                let s = 1.0 / (x1 - x0);
                let x0f = x0 - x0floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                self.acc[linestart + x0i as usize] += d * a0;
                if x1i == x0i + 2 {
                    self.acc[linestart + (x0i + 1) as usize] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    self.acc[linestart + (x0i + 1) as usize] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.acc[linestart + xi as usize] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.acc[linestart + (x1i - 1) as usize] += d * (1.0 - a2 - am);
                }
                self.acc[linestart + x1i as usize] += d * am;
            }
            x = xnext;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{Affine, Rect};
    use crate::path::Path;

    fn fill(path: &Path, rule: FillRule, clip: IRect) -> Mask {
        let mut f = Flat::new();
        path.flatten(&Affine::IDENTITY, 0.1, &mut f);
        Rasterizer::new().fill(&f, rule, clip)
    }

    #[test]
    fn pixel_aligned_rect_is_exact() {
        let mut p = Path::new();
        p.rect(Rect::new(2.0, 3.0, 4.0, 2.0));
        let m = fill(&p, FillRule::NonZero, IRect::new(0, 0, 20, 20));
        assert_eq!((m.x, m.y, m.w, m.h), (2, 3, 4, 2));
        assert!(m.data.iter().all(|v| *v == 255));
    }

    #[test]
    fn half_pixel_edges_give_half_coverage() {
        let mut p = Path::new();
        p.rect(Rect::new(0.5, 0.0, 2.0, 1.0));
        let m = fill(&p, FillRule::NonZero, IRect::new(0, 0, 8, 8));
        assert_eq!(m.get(0, 0), 128);
        assert_eq!(m.get(1, 0), 255);
        assert_eq!(m.get(2, 0), 128);
    }

    #[test]
    fn clipping_keeps_the_winding_of_left_cut_edges() {
        let mut p = Path::new();
        p.rect(Rect::new(-10.0, 0.0, 30.0, 4.0));
        let m = fill(&p, FillRule::NonZero, IRect::new(0, 0, 5, 4));
        assert_eq!((m.w, m.h), (5, 4));
        assert!(m.data.iter().all(|v| *v == 255));
    }

    #[test]
    fn even_odd_leaves_a_hole() {
        let mut p = Path::new();
        p.rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        p.rect(Rect::new(3.0, 3.0, 4.0, 4.0));
        let eo = fill(&p, FillRule::EvenOdd, IRect::new(0, 0, 10, 10));
        assert_eq!(eo.get(5, 5), 0);
        assert_eq!(eo.get(1, 1), 255);
        // same winding direction: non-zero fills the hole
        let nz = fill(&p, FillRule::NonZero, IRect::new(0, 0, 10, 10));
        assert_eq!(nz.get(5, 5), 255);
    }

    #[test]
    fn circle_area_matches() {
        let mut p = Path::new();
        p.circle(Point::new(20.0, 20.0), 10.0);
        let m = fill(&p, FillRule::NonZero, IRect::new(0, 0, 40, 40));
        let sum: f32 = m.data.iter().map(|v| *v as f32 / 255.0).sum();
        let expect = std::f32::consts::PI * 100.0;
        assert!((sum - expect).abs() / expect < 0.02, "area {sum} vs {expect}");
    }
}
