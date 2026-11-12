//! Points, rectangles, 2D affine transforms and colors.

use std::ops::{Add, Mul, Neg, Sub};

/// A point (or vector) in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    pub fn dot(self, o: Point) -> f32 {
        self.x * o.x + self.y * o.y
    }
    pub fn cross(self, o: Point) -> f32 {
        self.x * o.y - self.y * o.x
    }
    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }
    pub fn lerp(self, o: Point, t: f32) -> Point {
        Point::new(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }
    /// Unit vector, or zero for a zero vector.
    pub fn normalized(self) -> Point {
        let l = self.length();
        if l > 1e-12 { Point::new(self.x / l, self.y / l) } else { Point::ZERO }
    }
    /// Rotated by +90 degrees (x right, y down: clockwise on screen).
    pub fn perp(self) -> Point {
        Point::new(-self.y, self.x)
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}
impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}
impl Mul<f32> for Point {
    type Output = Point;
    fn mul(self, k: f32) -> Point {
        Point::new(self.x * k, self.y * k)
    }
}
impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

/// An axis-aligned rectangle: origin (top-left) and size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn from_ltrb(l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect { x: l, y: t, w: r - l, h: b - t }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
    pub fn is_empty(&self) -> bool {
        !(self.w > 0.0 && self.h > 0.0)
    }
    pub fn inflate(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }
    pub fn translate(&self, d: Point) -> Rect {
        Rect::new(self.x + d.x, self.y + d.y, self.w, self.h)
    }
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let l = self.x.max(o.x);
        let t = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        if r > l && b > t { Rect::from_ltrb(l, t, r, b) } else { Rect::new(l, t, 0.0, 0.0) }
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect::from_ltrb(self.x.min(o.x), self.y.min(o.y), self.right().max(o.right()), self.bottom().max(o.bottom()))
    }
}

/// A 2D affine transform: `x' = a*x + c*y + e`, `y' = b*x + d*y + f`, stored `[a, b, c, d, e, f]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine(pub [f32; 6]);

impl Default for Affine {
    fn default() -> Self {
        Affine::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Affine = Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    pub fn translate(x: f32, y: f32) -> Affine {
        Affine([1.0, 0.0, 0.0, 1.0, x, y])
    }
    pub fn scale(sx: f32, sy: f32) -> Affine {
        Affine([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }
    /// Rotation by `radians` (positive is clockwise on screen, y down).
    pub fn rotate(radians: f32) -> Affine {
        let (s, c) = radians.sin_cos();
        Affine([c, s, -s, c, 0.0, 0.0])
    }
    /// Apply `self` first, then `next`.
    pub fn then(&self, next: &Affine) -> Affine {
        let a = &self.0;
        let b = &next.0;
        Affine([
            b[0] * a[0] + b[2] * a[1],
            b[1] * a[0] + b[3] * a[1],
            b[0] * a[2] + b[2] * a[3],
            b[1] * a[2] + b[3] * a[3],
            b[0] * a[4] + b[2] * a[5] + b[4],
            b[1] * a[4] + b[3] * a[5] + b[5],
        ])
    }
    pub fn apply(&self, p: Point) -> Point {
        let m = &self.0;
        Point::new(m[0] * p.x + m[2] * p.y + m[4], m[1] * p.x + m[3] * p.y + m[5])
    }
    /// Transform a vector (no translation).
    pub fn apply_vec(&self, p: Point) -> Point {
        let m = &self.0;
        Point::new(m[0] * p.x + m[2] * p.y, m[1] * p.x + m[3] * p.y)
    }
    pub fn det(&self) -> f32 {
        self.0[0] * self.0[3] - self.0[1] * self.0[2]
    }
    pub fn invert(&self) -> Option<Affine> {
        let d = self.det();
        if !d.is_finite() || d.abs() < 1e-20 {
            return None;
        }
        let m = &self.0;
        let id = 1.0 / d;
        let a = m[3] * id;
        let b = -m[1] * id;
        let c = -m[2] * id;
        let dd = m[0] * id;
        Some(Affine([a, b, c, dd, -(a * m[4] + c * m[5]), -(b * m[4] + dd * m[5])]))
    }
    /// True when the transform keeps axes aligned (scale and translation only).
    pub fn is_scale_translate(&self) -> bool {
        self.0[1].abs() < 1e-6 && self.0[2].abs() < 1e-6
    }
    /// The mean scale factor, `sqrt(|det|)`.
    pub fn mean_scale(&self) -> f32 {
        self.det().abs().sqrt()
    }
    /// The larger of the two axis scale factors.
    pub fn max_scale(&self) -> f32 {
        let m = &self.0;
        (m[0].hypot(m[1])).max(m[2].hypot(m[3]))
    }
    /// Bounding box of a transformed rectangle.
    pub fn map_rect(&self, r: &Rect) -> Rect {
        let p = [
            self.apply(Point::new(r.x, r.y)),
            self.apply(Point::new(r.right(), r.y)),
            self.apply(Point::new(r.right(), r.bottom())),
            self.apply(Point::new(r.x, r.bottom())),
        ];
        let mut l = p[0].x;
        let mut rr = p[0].x;
        let mut t = p[0].y;
        let mut b = p[0].y;
        for q in &p[1..] {
            l = l.min(q.x);
            rr = rr.max(q.x);
            t = t.min(q.y);
            b = b.max(q.y);
        }
        Rect::from_ltrb(l, t, rr, b)
    }
}

/// A color in sRGB with straight (non-premultiplied) alpha; components 0..=1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Color = Color::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Color {
        Color { r, g, b, a }
    }
    pub const fn rgb8(r: u8, g: u8, b: u8) -> Color {
        Color::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
    }
    pub const fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0)
    }
    /// `0xRRGGBB`, opaque.
    pub const fn hex(rgb: u32) -> Color {
        Color::rgb8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }
    pub fn with_alpha(self, a: f32) -> Color {
        Color { a, ..self }
    }
    /// Premultiplied components.
    pub fn premul(self) -> [f32; 4] {
        [self.r * self.a, self.g * self.a, self.b * self.a, self.a]
    }
    pub fn lerp(self, o: Color, t: f32) -> Color {
        Color::new(
            self.r + (o.r - self.r) * t,
            self.g + (o.g - self.g) * t,
            self.b + (o.b - self.b) * t,
            self.a + (o.a - self.a) * t,
        )
    }
}

/// An integer pixel rectangle `[x0, x1) x [y0, y1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl IRect {
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> IRect {
        IRect { x0, y0, x1, y1 }
    }
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
    pub fn width(&self) -> i32 {
        (self.x1 - self.x0).max(0)
    }
    pub fn height(&self) -> i32 {
        (self.y1 - self.y0).max(0)
    }
    pub fn intersect(&self, o: &IRect) -> IRect {
        IRect::new(self.x0.max(o.x0), self.y0.max(o.y0), self.x1.min(o.x1), self.y1.min(o.y1))
    }
    /// Smallest integer rectangle containing `r`.
    pub fn outset(r: &Rect) -> IRect {
        IRect::new(r.x.floor() as i32, r.y.floor() as i32, r.right().ceil() as i32, r.bottom().ceil() as i32)
    }
    /// Integer rectangle with edges rounded to the nearest pixel.
    pub fn round(r: &Rect) -> IRect {
        IRect::new(r.x.round() as i32, r.y.round() as i32, r.right().round() as i32, r.bottom().round() as i32)
    }
}
