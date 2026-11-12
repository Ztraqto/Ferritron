//! Vector paths: building and flattening into polylines.

use crate::geom::{Affine, Point, Rect};

/// One path command; the points it uses are stored separately (see [`Path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// 1 point.
    Move,
    /// 1 point.
    Line,
    /// 2 points (control, end).
    Quad,
    /// 3 points (control, control, end).
    Cubic,
    /// 0 points.
    Close,
}

impl Verb {
    pub fn point_count(self) -> usize {
        match self {
            Verb::Move | Verb::Line => 1,
            Verb::Quad => 2,
            Verb::Cubic => 3,
            Verb::Close => 0,
        }
    }
}

/// A vector path made of lines, quadratic and cubic Beziers (arcs become cubics).
#[derive(Clone, Debug, Default)]
pub struct Path {
    pub(crate) verbs: Vec<Verb>,
    pub(crate) pts: Vec<Point>,
}

/// Fill rule for paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

const KAPPA: f32 = 0.552_284_75;

impl Path {
    pub fn new() -> Path {
        Path::default()
    }
    /// Remove all commands, keeping the allocation.
    pub fn clear(&mut self) {
        self.verbs.clear();
        self.pts.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }
    pub fn verbs(&self) -> &[Verb] {
        &self.verbs
    }
    pub fn points(&self) -> &[Point] {
        &self.pts
    }
    pub fn move_to(&mut self, p: Point) -> &mut Self {
        self.verbs.push(Verb::Move);
        self.pts.push(p);
        self
    }
    pub fn line_to(&mut self, p: Point) -> &mut Self {
        if self.verbs.is_empty() {
            return self.move_to(p);
        }
        self.verbs.push(Verb::Line);
        self.pts.push(p);
        self
    }
    pub fn quad_to(&mut self, c: Point, p: Point) -> &mut Self {
        if self.verbs.is_empty() {
            self.move_to(c);
        }
        self.verbs.push(Verb::Quad);
        self.pts.push(c);
        self.pts.push(p);
        self
    }
    pub fn cubic_to(&mut self, c1: Point, c2: Point, p: Point) -> &mut Self {
        if self.verbs.is_empty() {
            self.move_to(c1);
        }
        self.verbs.push(Verb::Cubic);
        self.pts.push(c1);
        self.pts.push(c2);
        self.pts.push(p);
        self
    }
    pub fn close(&mut self) -> &mut Self {
        if !self.verbs.is_empty() && *self.verbs.last().unwrap() != Verb::Close {
            self.verbs.push(Verb::Close);
        }
        self
    }

    /// Append an elliptical arc starting at angle `start` (radians, 0 = +x, clockwise on screen)
    /// and sweeping by `sweep` radians. Continues from the current point with a line when the
    /// path is not empty (use `move_to` first to start a new sub-path).
    pub fn arc(&mut self, center: Point, rx: f32, ry: f32, start: f32, sweep: f32) -> &mut Self {
        let n = ((sweep.abs() / (std::f32::consts::FRAC_PI_2)).ceil() as usize).max(1);
        let step = sweep / n as f32;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let pt = |a: f32| Point::new(center.x + rx * a.cos(), center.y + ry * a.sin());
        let d = |a: f32| Point::new(-rx * a.sin(), ry * a.cos());
        let first = pt(start);
        if self.verbs.is_empty() || *self.verbs.last().unwrap() == Verb::Close {
            self.move_to(first);
        } else {
            self.line_to(first);
        }
        let mut a0 = start;
        for _ in 0..n {
            let a1 = a0 + step;
            let p0 = pt(a0);
            let p1 = pt(a1);
            let d0 = d(a0);
            let d1 = d(a1);
            self.cubic_to(p0 + d0 * k, p1 - d1 * k, p1);
            a0 = a1;
        }
        self
    }

    pub fn rect(&mut self, r: Rect) -> &mut Self {
        self.move_to(Point::new(r.x, r.y));
        self.line_to(Point::new(r.right(), r.y));
        self.line_to(Point::new(r.right(), r.bottom()));
        self.line_to(Point::new(r.x, r.bottom()));
        self.close()
    }

    pub fn round_rect(&mut self, r: Rect, radius: f32) -> &mut Self {
        let rad = radius.max(0.0).min(r.w * 0.5).min(r.h * 0.5);
        if rad <= 0.0 {
            return self.rect(r);
        }
        let k = rad * KAPPA;
        let (l, t, rt, b) = (r.x, r.y, r.right(), r.bottom());
        self.move_to(Point::new(l + rad, t));
        self.line_to(Point::new(rt - rad, t));
        self.cubic_to(Point::new(rt - rad + k, t), Point::new(rt, t + rad - k), Point::new(rt, t + rad));
        self.line_to(Point::new(rt, b - rad));
        self.cubic_to(Point::new(rt, b - rad + k), Point::new(rt - rad + k, b), Point::new(rt - rad, b));
        self.line_to(Point::new(l + rad, b));
        self.cubic_to(Point::new(l + rad - k, b), Point::new(l, b - rad + k), Point::new(l, b - rad));
        self.line_to(Point::new(l, t + rad));
        self.cubic_to(Point::new(l, t + rad - k), Point::new(l + rad - k, t), Point::new(l + rad, t));
        self.close()
    }

    pub fn ellipse(&mut self, c: Point, rx: f32, ry: f32) -> &mut Self {
        let (kx, ky) = (rx * KAPPA, ry * KAPPA);
        self.move_to(Point::new(c.x + rx, c.y));
        self.cubic_to(Point::new(c.x + rx, c.y + ky), Point::new(c.x + kx, c.y + ry), Point::new(c.x, c.y + ry));
        self.cubic_to(Point::new(c.x - kx, c.y + ry), Point::new(c.x - rx, c.y + ky), Point::new(c.x - rx, c.y));
        self.cubic_to(Point::new(c.x - rx, c.y - ky), Point::new(c.x - kx, c.y - ry), Point::new(c.x, c.y - ry));
        self.cubic_to(Point::new(c.x + kx, c.y - ry), Point::new(c.x + rx, c.y - ky), Point::new(c.x + rx, c.y));
        self.close()
    }

    pub fn circle(&mut self, c: Point, r: f32) -> &mut Self {
        self.ellipse(c, r, r)
    }

    /// Append another path.
    pub fn extend(&mut self, other: &Path) -> &mut Self {
        self.verbs.extend_from_slice(&other.verbs);
        self.pts.extend_from_slice(&other.pts);
        self
    }

    /// Bounding box of all control points (a conservative bound for the curve).
    pub fn bounds(&self) -> Rect {
        points_bounds(&self.pts)
    }

    /// A transformed copy.
    pub fn transformed(&self, xf: &Affine) -> Path {
        Path { verbs: self.verbs.clone(), pts: self.pts.iter().map(|p| xf.apply(*p)).collect() }
    }

    /// Flatten into polylines after applying `xf`, within `tolerance` (in the output units).
    pub fn flatten(&self, xf: &Affine, tolerance: f32, out: &mut Flat) {
        flatten_raw(&self.verbs, &self.pts, xf, tolerance, out);
    }
}

pub(crate) fn points_bounds(pts: &[Point]) -> Rect {
    if pts.is_empty() {
        return Rect::default();
    }
    let (mut l, mut t, mut r, mut b) = (pts[0].x, pts[0].y, pts[0].x, pts[0].y);
    for p in &pts[1..] {
        l = l.min(p.x);
        r = r.max(p.x);
        t = t.min(p.y);
        b = b.max(p.y);
    }
    Rect::from_ltrb(l, t, r, b)
}

/// One flattened sub-path: a range of [`Flat::pts`].
#[derive(Clone, Copy, Debug)]
pub struct SubPath {
    pub start: u32,
    pub end: u32,
    pub closed: bool,
}

/// A path flattened into polylines.
#[derive(Clone, Debug, Default)]
pub struct Flat {
    pub pts: Vec<Point>,
    pub subs: Vec<SubPath>,
}

impl Flat {
    pub fn new() -> Flat {
        Flat::default()
    }
    pub fn clear(&mut self) {
        self.pts.clear();
        self.subs.clear();
    }
    pub fn sub_points(&self, s: &SubPath) -> &[Point] {
        &self.pts[s.start as usize..s.end as usize]
    }
    pub fn bounds(&self) -> Rect {
        points_bounds(&self.pts)
    }
    pub(crate) fn begin(&mut self, p: Point) {
        self.finish_sub(false);
        self.pts.push(p);
    }
    pub(crate) fn push(&mut self, p: Point) {
        self.pts.push(p);
    }
    /// Close the sub-path in progress, if any.
    pub(crate) fn finish_sub(&mut self, closed: bool) {
        let start = self.subs.last().map_or(0, |s| s.end);
        let end = self.pts.len() as u32;
        if end > start {
            self.subs.push(SubPath { start, end, closed });
        }
    }
}

pub(crate) fn flatten_raw(verbs: &[Verb], pts: &[Point], xf: &Affine, tol: f32, out: &mut Flat) {
    out.clear();
    let tol = tol.max(1e-4);
    let mut i = 0usize;
    let mut cur = Point::ZERO;
    let mut start = Point::ZERO;
    let mut open = false;
    for v in verbs {
        match v {
            Verb::Move => {
                let p = xf.apply(pts[i]);
                i += 1;
                if open {
                    out.finish_sub(false);
                }
                out.begin(p);
                open = true;
                cur = p;
                start = p;
            }
            Verb::Line => {
                let p = xf.apply(pts[i]);
                i += 1;
                if !open {
                    out.begin(cur);
                    open = true;
                }
                out.push(p);
                cur = p;
            }
            Verb::Quad => {
                let c = xf.apply(pts[i]);
                let p = xf.apply(pts[i + 1]);
                i += 2;
                if !open {
                    out.begin(cur);
                    open = true;
                }
                let dd = (cur - c * 2.0 + p).length();
                let n = ((dd / (4.0 * tol)).sqrt().ceil() as usize).clamp(1, 500);
                for k in 1..=n {
                    let t = k as f32 / n as f32;
                    let a = cur.lerp(c, t);
                    let b = c.lerp(p, t);
                    out.push(a.lerp(b, t));
                }
                cur = p;
            }
            Verb::Cubic => {
                let c1 = xf.apply(pts[i]);
                let c2 = xf.apply(pts[i + 1]);
                let p = xf.apply(pts[i + 2]);
                i += 3;
                if !open {
                    out.begin(cur);
                    open = true;
                }
                let dd = (cur - c1 * 2.0 + c2).length().max((c1 - c2 * 2.0 + p).length());
                let n = ((0.75 * dd / tol).sqrt().ceil() as usize).clamp(1, 500);
                for k in 1..=n {
                    let t = k as f32 / n as f32;
                    let mt = 1.0 - t;
                    let x = mt * mt * mt * cur.x + 3.0 * mt * mt * t * c1.x + 3.0 * mt * t * t * c2.x + t * t * t * p.x;
                    let y = mt * mt * mt * cur.y + 3.0 * mt * mt * t * c1.y + 3.0 * mt * t * t * c2.y + t * t * t * p.y;
                    out.push(Point::new(x, y));
                }
                cur = p;
            }
            Verb::Close => {
                if open {
                    out.finish_sub(true);
                    open = false;
                    cur = start;
                }
            }
        }
    }
    if open {
        out.finish_sub(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_flattens_to_four_points() {
        let mut p = Path::new();
        p.rect(Rect::new(1.0, 2.0, 10.0, 5.0));
        let mut f = Flat::new();
        p.flatten(&Affine::IDENTITY, 0.25, &mut f);
        assert_eq!(f.subs.len(), 1);
        assert_eq!(f.subs[0].end - f.subs[0].start, 4);
        assert!(f.subs[0].closed);
    }

    #[test]
    fn circle_stays_within_tolerance() {
        let mut p = Path::new();
        p.circle(Point::new(50.0, 50.0), 40.0);
        let mut f = Flat::new();
        p.flatten(&Affine::IDENTITY, 0.25, &mut f);
        for q in &f.pts {
            let d = ((q.x - 50.0).powi(2) + (q.y - 50.0).powi(2)).sqrt();
            assert!((d - 40.0).abs() < 0.3, "vertex off the circle by {}", (d - 40.0).abs());
        }
        // chords may sag, but never more than the tolerance.
        for w in f.pts.windows(2) {
            let m = w[0].lerp(w[1], 0.5);
            let d = ((m.x - 50.0).powi(2) + (m.y - 50.0).powi(2)).sqrt();
            assert!(40.0 - d < 0.3);
        }
    }

    #[test]
    fn arc_ends_at_the_expected_point() {
        let mut p = Path::new();
        p.arc(Point::new(0.0, 0.0), 10.0, 10.0, 0.0, std::f32::consts::PI);
        let e = *p.points().last().unwrap();
        assert!((e.x + 10.0).abs() < 1e-3 && e.y.abs() < 1e-3);
    }
}
