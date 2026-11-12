//! Strokes: line style types and the stroker that turns a polyline into fillable polygons.

use crate::geom::{Affine, Point};
use crate::path::Flat;

/// How two segments of a stroke meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    Miter,
    Round,
    Bevel,
}

/// How the ends of an open stroke look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Butt,
    Round,
    Square,
}

/// A dash pattern: alternating on/off lengths (logical pixels), up to 6 entries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dash {
    pub pattern: [f32; 6],
    pub len: u8,
    pub phase: f32,
}

impl Dash {
    /// `on` pixels drawn, `off` pixels skipped, repeated.
    pub fn new(on: f32, off: f32) -> Dash {
        Dash { pattern: [on, off, 0.0, 0.0, 0.0, 0.0], len: 2, phase: 0.0 }
    }
    /// A longer pattern (`on, off, on, off, ...`); an odd count is repeated to make it even. Extra
    /// entries beyond 6 are ignored.
    pub fn from_pattern(p: &[f32]) -> Dash {
        let mut pattern = [0.0; 6];
        let mut len = 0usize;
        let n = if p.len() % 2 == 1 && p.len() * 2 <= 6 { p.len() * 2 } else { p.len().min(6) & !1 };
        for i in 0..n {
            pattern[i] = p[i % p.len()];
            len += 1;
        }
        Dash { pattern, len: len as u8, phase: 0.0 }
    }
    pub fn with_phase(mut self, phase: f32) -> Dash {
        self.phase = phase;
        self
    }
    pub(crate) fn slice(&self) -> &[f32] {
        &self.pattern[..self.len as usize]
    }
}

/// Line style for stroking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub join: Join,
    pub cap: Cap,
    /// Largest miter length (as a multiple of half the width) before a miter turns into a bevel.
    pub miter_limit: f32,
    pub dash: Option<Dash>,
}

impl Stroke {
    /// A solid stroke of the given width: miter joins, butt caps.
    pub fn width(width: f32) -> Stroke {
        Stroke { width, join: Join::Miter, cap: Cap::Butt, miter_limit: 4.0, dash: None }
    }
    pub fn with_join(mut self, join: Join) -> Stroke {
        self.join = join;
        self
    }
    pub fn with_cap(mut self, cap: Cap) -> Stroke {
        self.cap = cap;
        self
    }
    pub fn with_dash(mut self, dash: Dash) -> Stroke {
        self.dash = Some(dash);
        self
    }
    pub fn with_miter_limit(mut self, limit: f32) -> Stroke {
        self.miter_limit = limit;
        self
    }
}

impl Default for Stroke {
    fn default() -> Self {
        Stroke::width(1.0)
    }
}

/// Split every sub-path of `src` into dashes (appended to `out`, which is cleared first).
pub fn dash_flat(src: &Flat, dash: &Dash, out: &mut Flat) {
    out.clear();
    let pat = dash.slice();
    let total: f32 = pat.iter().sum();
    if pat.len() < 2 || !(total > 1e-6) || pat.iter().any(|v| *v < 0.0) {
        out.pts.extend_from_slice(&src.pts);
        out.subs.extend_from_slice(&src.subs);
        return;
    }
    let mut budget = 200_000usize;
    for s in &src.subs {
        let pts = src.sub_points(s);
        if pts.len() < 2 && !s.closed {
            continue;
        }
        // phase -> starting pattern index and remaining length
        let mut ph = dash.phase.rem_euclid(total);
        let mut idx = 0usize;
        while ph >= pat[idx] && ph > 0.0 {
            ph -= pat[idx];
            idx = (idx + 1) % pat.len();
            if pat[idx] == 0.0 && ph == 0.0 {
                break;
            }
        }
        let mut remain = pat[idx] - ph;
        let mut on = idx % 2 == 0;
        let n = pts.len() + usize::from(s.closed);
        let mut open_piece = false;
        for k in 1..n {
            let a = pts[k - 1];
            let b = pts[k % pts.len()];
            let seg = (b - a).length();
            if seg <= 1e-9 {
                continue;
            }
            if on && !open_piece {
                out.begin(a);
                open_piece = true;
            }
            let mut pos = 0.0f32;
            while seg - pos > remain {
                pos += remain;
                let p = a.lerp(b, pos / seg);
                if on {
                    out.push(p);
                    out.finish_sub(false);
                    open_piece = false;
                } else {
                    out.begin(p);
                    open_piece = true;
                }
                on = !on;
                idx = (idx + 1) % pat.len();
                remain = pat[idx];
                if remain <= 0.0 {
                    // zero-length entry: flip again to avoid an endless loop
                    on = !on;
                    idx = (idx + 1) % pat.len();
                    remain = pat[idx].max(1e-4);
                }
                budget = budget.saturating_sub(1);
                if budget == 0 {
                    return;
                }
            }
            remain -= seg - pos;
            if on && open_piece {
                out.push(b);
            }
        }
        if open_piece {
            out.finish_sub(false);
        }
    }
}

/// Build the outline polygons of a stroke. `local` holds polylines in local space; the
/// polygons are written to `out` in device space (`xf` applied), each with a positive winding
/// so that a non-zero fill of all of them gives the union. `tol` is in local units.
pub fn stroke_polys(local: &Flat, stroke: &Stroke, xf: &Affine, tol: f32, out: &mut Flat) {
    stroke_polys_with(local, stroke, xf, tol, out, &mut StrokeScratch::default());
}

/// The stroker's working buffers. A renderer keeps one, so that stroking allocates nothing
/// once the buffers have grown to the largest stroke.
#[derive(Default)]
pub(crate) struct StrokeScratch {
    tmp: Vec<Point>,
    poly: Vec<Point>,
}

/// [`stroke_polys`] with the caller's buffers (see [`StrokeScratch`]).
pub(crate) fn stroke_polys_with(local: &Flat, stroke: &Stroke, xf: &Affine, tol: f32, out: &mut Flat, scratch: &mut StrokeScratch) {
    out.clear();
    let hw = stroke.width * 0.5;
    if !(hw > 0.0) {
        return;
    }
    let StrokeScratch { tmp, poly } = scratch;
    let emit = |poly: &[Point], out: &mut Flat| {
        if poly.len() < 3 {
            return;
        }
        let start = out.pts.len();
        for p in poly {
            out.pts.push(xf.apply(*p));
        }
        // orientation: make every piece wind the same way.
        let mut area = 0.0f32;
        let q = &out.pts[start..];
        for i in 0..q.len() {
            area += q[i].cross(q[(i + 1) % q.len()]);
        }
        if area < 0.0 {
            out.pts[start..].reverse();
        }
        out.finish_sub(true);
    };
    let step = {
        let r = hw.max(1e-6);
        (2.0 * (1.0 - (tol / r).min(1.0)).acos()).clamp(0.04, std::f32::consts::FRAC_PI_2)
    };
    for s in &local.subs {
        // remove duplicate points
        tmp.clear();
        for p in local.sub_points(s) {
            if tmp.last().is_none_or(|l| (*l - *p).length() > 1e-6) {
                tmp.push(*p);
            }
        }
        let closed = s.closed && tmp.len() >= 3;
        if closed && (tmp[0] - *tmp.last().unwrap()).length() <= 1e-6 {
            tmp.pop();
        }
        if tmp.len() == 1 {
            let p = tmp[0];
            match stroke.cap {
                Cap::Round => {
                    poly.clear();
                    let n = ((std::f32::consts::TAU / step).ceil() as usize).max(8);
                    for i in 0..n {
                        let a = i as f32 / n as f32 * std::f32::consts::TAU;
                        poly.push(Point::new(p.x + hw * a.cos(), p.y + hw * a.sin()));
                    }
                    emit(poly.as_slice(), out);
                }
                Cap::Square => {
                    emit(&[Point::new(p.x - hw, p.y - hw), Point::new(p.x + hw, p.y - hw), Point::new(p.x + hw, p.y + hw), Point::new(p.x - hw, p.y + hw)], out);
                }
                Cap::Butt => {}
            }
            continue;
        }
        if tmp.len() < 2 {
            continue;
        }
        let n = tmp.len();
        let segs = if closed { n } else { n - 1 };
        // segment quads
        for i in 0..segs {
            let a = tmp[i];
            let b = tmp[(i + 1) % n];
            let d = (b - a).normalized();
            let nn = d.perp() * hw;
            emit(&[a + nn, b + nn, b - nn, a - nn], out);
        }
        // joins
        let join_range = if closed { 0..n } else { 1..n - 1 };
        for i in join_range {
            let prev = tmp[(i + n - 1) % n];
            let v = tmp[i];
            let next = tmp[(i + 1) % n];
            let d0 = (v - prev).normalized();
            let d1 = (next - v).normalized();
            let cross = d0.cross(d1);
            let dot = d0.dot(d1);
            if cross.abs() < 1e-6 && dot > 0.0 {
                continue;
            }
            let sgn = if cross >= 0.0 { -1.0 } else { 1.0 };
            let n0 = d0.perp() * (hw * sgn);
            let n1 = d1.perp() * (hw * sgn);
            let a = v + n0;
            let b = v + n1;
            match stroke.join {
                Join::Bevel => emit(&[v, a, b], out),
                Join::Miter => {
                    let denom = 1.0 + dot;
                    let ratio = if denom > 1e-6 { (2.0 / denom).sqrt() } else { f32::INFINITY };
                    if ratio <= stroke.miter_limit && denom > 1e-6 {
                        let tip = v + (n0 + n1) * (1.0 / denom);
                        emit(&[v, a, tip, b], out);
                    } else {
                        emit(&[v, a, b], out);
                    }
                }
                Join::Round => {
                    let a0 = n0.y.atan2(n0.x);
                    let sweep = n0.cross(n1).atan2(n0.dot(n1));
                    let sweep = if cross.abs() < 1e-6 && dot < 0.0 { std::f32::consts::PI } else { sweep };
                    let steps = ((sweep.abs() / step).ceil() as usize).max(1);
                    poly.clear();
                    poly.push(v);
                    poly.push(a);
                    for k in 1..steps {
                        let ang = a0 + sweep * k as f32 / steps as f32;
                        poly.push(Point::new(v.x + hw * ang.cos(), v.y + hw * ang.sin()));
                    }
                    poly.push(b);
                    emit(poly.as_slice(), out);
                }
            }
        }
        // caps
        if !closed {
            for end in 0..2 {
                let (p, d) = if end == 0 { (tmp[0], (tmp[1] - tmp[0]).normalized()) } else { (tmp[n - 1], (tmp[n - 1] - tmp[n - 2]).normalized()) };
                let nn = d.perp() * hw;
                // outward direction
                let out_dir = if end == 0 { -d } else { d };
                match stroke.cap {
                    Cap::Butt => {}
                    Cap::Square => emit(&[p + nn, p + nn + out_dir * hw, p - nn + out_dir * hw, p - nn], out),
                    Cap::Round => {
                        let a0 = nn.y.atan2(nn.x);
                        let sweep = if end == 0 { std::f32::consts::PI } else { -std::f32::consts::PI };
                        let steps = ((sweep.abs() / step).ceil() as usize).max(2);
                        poly.clear();
                        for k in 0..=steps {
                            let ang = a0 + sweep * k as f32 / steps as f32;
                            poly.push(Point::new(p.x + hw * ang.cos(), p.y + hw * ang.sin()));
                        }
                        emit(poly.as_slice(), out);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::SubPath;

    fn flat_line(a: Point, b: Point) -> Flat {
        Flat { pts: vec![a, b], subs: vec![SubPath { start: 0, end: 2, closed: false }] }
    }

    #[test]
    fn dash_splits_a_line() {
        let src = flat_line(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        let mut out = Flat::new();
        dash_flat(&src, &Dash::new(2.0, 2.0), &mut out);
        assert_eq!(out.subs.len(), 3, "0-2, 4-6, 8-10");
        let first = out.sub_points(&out.subs[0]);
        assert!((first[0].x).abs() < 1e-5 && (first[1].x - 2.0).abs() < 1e-5);
        let last = out.sub_points(&out.subs[2]);
        assert!((last[0].x - 8.0).abs() < 1e-5 && (last[1].x - 10.0).abs() < 1e-5);
    }

    #[test]
    fn dash_phase_shifts_the_pattern() {
        let src = flat_line(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        let mut out = Flat::new();
        dash_flat(&src, &Dash::new(2.0, 2.0).with_phase(1.0), &mut out);
        let first = out.sub_points(&out.subs[0]);
        assert!((first[1].x - 1.0).abs() < 1e-5, "first dash is the last pixel of an on-span");
    }

    #[test]
    fn butt_stroke_of_a_line_is_one_quad() {
        let src = flat_line(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        let mut out = Flat::new();
        stroke_polys(&src, &Stroke::width(4.0), &Affine::IDENTITY, 0.1, &mut out);
        assert_eq!(out.subs.len(), 1);
        let b = out.bounds();
        assert!((b.w - 10.0).abs() < 1e-5 && (b.h - 4.0).abs() < 1e-5);
    }

    #[test]
    fn square_caps_extend_by_half_width() {
        let src = flat_line(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        let mut out = Flat::new();
        stroke_polys(&src, &Stroke::width(4.0).with_cap(Cap::Square), &Affine::IDENTITY, 0.1, &mut out);
        let b = out.bounds();
        assert!((b.x + 2.0).abs() < 1e-5 && (b.w - 14.0).abs() < 1e-5);
    }
}
