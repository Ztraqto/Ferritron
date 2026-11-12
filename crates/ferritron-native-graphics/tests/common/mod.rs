//! Scenes and helpers shared by the integration tests.
#![allow(dead_code)]

use fng::*;
use std::path::PathBuf;

pub const BG: Color = Color::hex(0x20242c);
pub const FG: Color = Color::hex(0xe8ecf4);
pub const ACCENT: Color = Color::hex(0xff9f1c);
pub const BLUE: Color = Color::hex(0x2ec4b6);
pub const RED: Color = Color::hex(0xe71d36);

/// Logical size of every test scene.
pub const W: u32 = 256;
pub const H: u32 = 192;

/// A small 8x8 test image (checker with a gradient and a transparent corner), straight alpha.
pub fn test_image() -> (Vec<u8>, u32, u32) {
    let (w, h) = (8u32, 8u32);
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let checker = (x + y) % 2 == 0;
            let a = if x < 2 && y < 2 { 0 } else { 255 };
            if checker {
                v.extend_from_slice(&[(x * 32) as u8, 200, (y * 32) as u8, a]);
            } else {
                v.extend_from_slice(&[255, (y * 32) as u8, (x * 32) as u8, a]);
            }
        }
    }
    (v, w, h)
}

/// Basic shapes: rectangles, rounded rectangles, circles, lines with every cap, polyline.
pub fn scene_shapes() -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    s.fill_rect(Rect::new(10.0, 10.0, 40.0, 30.0), ACCENT);
    s.fill_rect(Rect::new(60.5, 10.5, 40.0, 30.0), BLUE);
    s.fill_round_rect(Rect::new(110.0, 10.0, 60.0, 30.0), 8.0, RED);
    s.fill_round_rect(Rect::new(180.0, 10.0, 60.0, 30.0), 15.0, FG);
    s.fill_circle(Point::new(30.0, 75.0), 20.0, ACCENT);
    s.fill_ellipse(Point::new(85.0, 75.0), 28.0, 16.0, BLUE);
    s.fill_circle(Point::new(140.3, 75.7), 15.0, Color::WHITE.with_alpha(0.5));
    s.fill_circle(Point::new(150.3, 75.7), 15.0, RED.with_alpha(0.5));
    s.stroke_line(Point::new(185.0, 60.0), Point::new(240.0, 60.0), &Stroke::width(8.0), FG);
    s.stroke_line(Point::new(185.0, 76.0), Point::new(240.0, 76.0), &Stroke::width(8.0).with_cap(Cap::Square), ACCENT);
    s.stroke_line(Point::new(185.0, 92.0), Point::new(240.0, 92.0), &Stroke::width(8.0).with_cap(Cap::Round), BLUE);
    s.stroke_line(Point::new(185.0, 100.0), Point::new(240.0, 112.0), &Stroke::width(1.0), RED);
    let mut pts = Vec::new();
    for i in 0..40 {
        let x = 10.0 + i as f32 * 5.0;
        let y = 140.0 + (i as f32 * 0.5).sin() * 25.0;
        pts.push(Point::new(x, y));
    }
    s.stroke_polyline(&pts, &Stroke::width(2.0).with_cap(Cap::Round), FG);
    s.stroke_line(Point::new(10.0, 180.0), Point::new(240.0, 180.0), &Stroke::width(2.0).with_dash(Dash::new(8.0, 4.0)), ACCENT);
    s
}

/// Paths: fill rules, curves, strokes with every join, dashed curves.
pub fn scene_paths() -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    // star, non-zero and even-odd
    let star = |cx: f32, cy: f32, r: f32| {
        let mut p = Path::new();
        for i in 0..5 {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * 4.0 * std::f32::consts::PI / 5.0;
            let pt = Point::new(cx + r * a.cos(), cy + r * a.sin());
            if i == 0 {
                p.move_to(pt);
            } else {
                p.line_to(pt);
            }
        }
        p.close();
        p
    };
    s.fill_path(&star(40.0, 40.0, 32.0), FillRule::NonZero, ACCENT);
    s.fill_path(&star(110.0, 40.0, 32.0), FillRule::EvenOdd, BLUE);
    // curves
    let mut p = Path::new();
    p.move_to(Point::new(160.0, 70.0));
    p.cubic_to(Point::new(170.0, 0.0), Point::new(210.0, 0.0), Point::new(220.0, 40.0));
    p.quad_to(Point::new(230.0, 70.0), Point::new(240.0, 20.0));
    p.line_to(Point::new(240.0, 70.0));
    p.close();
    s.fill_path(&p, FillRule::NonZero, RED);
    // ring with a hole
    let mut ring = Path::new();
    ring.circle(Point::new(40.0, 120.0), 28.0);
    ring.circle(Point::new(40.0, 120.0), 16.0);
    s.fill_path(&ring, FillRule::EvenOdd, FG);
    // joins
    let zig = |x: f32| {
        let mut p = Path::new();
        p.move_to(Point::new(x, 150.0));
        p.line_to(Point::new(x + 20.0, 105.0));
        p.line_to(Point::new(x + 40.0, 150.0));
        p
    };
    s.stroke_path(&zig(80.0), &Stroke::width(10.0).with_join(Join::Miter), ACCENT);
    s.stroke_path(&zig(130.0), &Stroke::width(10.0).with_join(Join::Round).with_cap(Cap::Round), BLUE);
    s.stroke_path(&zig(180.0), &Stroke::width(10.0).with_join(Join::Bevel).with_cap(Cap::Square), RED);
    // dashed curve and closed outline
    let mut c = Path::new();
    c.move_to(Point::new(10.0, 185.0));
    c.cubic_to(Point::new(60.0, 150.0), Point::new(110.0, 215.0), Point::new(160.0, 180.0));
    s.stroke_path(&c, &Stroke::width(3.0).with_dash(Dash::new(10.0, 5.0)).with_cap(Cap::Round), FG);
    s.stroke_round_rect(Rect::new(176.0, 160.0, 70.0, 26.0), 8.0, &Stroke::width(3.0), ACCENT);
    s
}

/// Gradients, images and shadows.
pub fn scene_paints() -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    let stops = Stops::new(&[(0.0, ACCENT), (0.5, RED), (1.0, BLUE)]);
    s.fill_rect(Rect::new(10.0, 10.0, 110.0, 30.0), Paint::Linear { start: Point::new(10.0, 0.0), end: Point::new(120.0, 0.0), stops });
    s.fill_round_rect(
        Rect::new(130.0, 10.0, 110.0, 30.0),
        10.0,
        Paint::Linear { start: Point::new(130.0, 10.0), end: Point::new(240.0, 40.0), stops: Stops::two(Color::WHITE, Color::hex(0x3a86ff).with_alpha(0.3)) },
    );
    s.fill_circle(
        Point::new(40.0, 85.0),
        28.0,
        Paint::Radial { center: Point::new(32.0, 77.0), radius: 36.0, stops: Stops::two(Color::WHITE, Color::hex(0x7209b7)) },
    );
    // images: stretched (bilinear and nearest) and as a paint on a circle
    s.image(ImageId(0), Rect::new(85.0, 55.0, 60.0, 60.0));
    s.image_sampled(ImageId(0), Rect::new(155.0, 55.0, 40.0, 40.0), Sampling::Nearest);
    s.fill_circle(
        Point::new(222.0, 80.0),
        20.0,
        Paint::Image { image: ImageId(0), transform: Affine::scale(5.0, 5.0).then(&Affine::translate(202.0, 60.0)), sampling: Sampling::Nearest },
    );
    // shadows
    s.shadow(Rect::new(20.0, 130.0, 60.0, 40.0), 8.0, ShadowStyle { blur: 16.0, offset: Point::new(0.0, 4.0), color: Color::BLACK.with_alpha(0.7) });
    s.fill_round_rect(Rect::new(20.0, 130.0, 60.0, 40.0), 8.0, Color::hex(0x3d4455));
    s.shadow(Rect::new(110.0, 135.0, 30.0, 30.0), 15.0, ShadowStyle { blur: 10.0, offset: Point::new(3.0, 3.0), color: ACCENT.with_alpha(0.9) });
    s.fill_circle(Point::new(125.0, 150.0), 15.0, FG);
    s
}

/// Clips, transforms, opacity groups and cached layers.
pub fn scene_state() -> Scene {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, W as f32, H as f32), BG);
    // rectangular clip
    s.push_clip_rect(Rect::new(10.0, 10.0, 60.0, 40.0));
    s.fill_circle(Point::new(30.0, 30.0), 40.0, ACCENT);
    s.pop_clip();
    // rounded clip around a striped fill
    s.push_clip_round_rect(Rect::new(80.0, 10.0, 70.0, 40.0), 14.0);
    for i in 0..8 {
        let c = if i % 2 == 0 { BLUE } else { FG };
        s.fill_rect(Rect::new(80.0 + i as f32 * 10.0, 10.0, 10.0, 40.0), c);
    }
    s.pop_clip();
    // path clip (a star) with a gradient inside
    let mut star = Path::new();
    for i in 0..5 {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * 4.0 * std::f32::consts::PI / 5.0;
        let pt = Point::new(200.0 + 32.0 * a.cos(), 32.0 + 32.0 * a.sin());
        if i == 0 {
            star.move_to(pt);
        } else {
            star.line_to(pt);
        }
    }
    star.close();
    s.push_clip_path(&star, FillRule::NonZero);
    s.fill_rect(
        Rect::new(160.0, 0.0, 80.0, 70.0),
        Paint::Linear { start: Point::new(160.0, 0.0), end: Point::new(240.0, 70.0), stops: Stops::two(RED, BLUE) },
    );
    s.pop_clip();
    // transform: rotated rounded rect, and a rotated clip
    s.push_transform(Affine::rotate(0.5).then(&Affine::translate(60.0, 110.0)));
    s.fill_round_rect(Rect::new(-30.0, -20.0, 60.0, 40.0), 6.0, ACCENT);
    s.stroke_line(Point::new(-30.0, 0.0), Point::new(30.0, 0.0), &Stroke::width(2.0), BG);
    s.pop_transform();
    s.push_transform(Affine::rotate(-0.3).then(&Affine::translate(130.0, 120.0)));
    s.push_clip_rect(Rect::new(-25.0, -20.0, 50.0, 40.0));
    s.fill_circle(Point::new(0.0, 0.0), 30.0, BLUE);
    s.pop_clip();
    s.pop_transform();
    // opacity group: two overlapping circles drawn as one
    s.push_opacity(0.5);
    s.fill_circle(Point::new(190.0, 120.0), 22.0, RED);
    s.fill_circle(Point::new(210.0, 120.0), 22.0, FG);
    s.pop_opacity();
    // cached layer drawn twice
    for (i, x) in [20.0f32, 100.0].iter().enumerate() {
        s.push_transform(Affine::translate(*x, 150.0));
        s.layer(LayerId(1), (60.0, 30.0), 1, |l| {
            l.fill_round_rect(Rect::new(0.0, 0.0, 60.0, 30.0), 6.0, Color::hex(0x3d4455));
            l.fill_circle(Point::new(15.0, 15.0), 8.0, ACCENT);
            l.stroke_line(Point::new(30.0, 15.0), Point::new(52.0, 15.0), &Stroke::width(3.0).with_cap(Cap::Round), FG);
        });
        s.pop_transform();
        let _ = i;
    }
    s
}

fn reference_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("reference")
}

/// Largest per-channel difference and the mean absolute difference between two images.
pub fn diff(a: &RgbaImage, b: &RgbaImage) -> (u8, f64) {
    assert_eq!((a.width, a.height), (b.width, b.height), "image sizes differ");
    let mut max = 0u8;
    let mut sum = 0u64;
    for (x, y) in a.data.iter().zip(&b.data) {
        let d = x.abs_diff(*y);
        max = max.max(d);
        sum += d as u64;
    }
    (max, sum as f64 / a.data.len() as f64)
}

/// Compare with the stored reference picture `tests/reference/<name>.png`. Set
/// `FNG_UPDATE_REFERENCE=1` to (re)write the references.
pub fn check_reference(name: &str, img: &RgbaImage, max_allowed: u8, mean_allowed: f64) {
    let path = reference_dir().join(format!("{name}.png"));
    if std::env::var("FNG_UPDATE_REFERENCE").is_ok() {
        std::fs::create_dir_all(reference_dir()).unwrap();
        img.save_png(&path).unwrap();
        return;
    }
    let reference = RgbaImage::load_png(&path)
        .unwrap_or_else(|e| panic!("missing reference picture {} ({e}); run once with FNG_UPDATE_REFERENCE=1", path.display()));
    let (max, mean) = diff(img, &reference);
    if max > max_allowed || mean > mean_allowed {
        let out = std::env::temp_dir().join(format!("fng-actual-{name}.png"));
        let _ = img.save_png(&out);
        panic!("{name}: differs from the reference (max {max} > {max_allowed} or mean {mean:.4} > {mean_allowed}); actual saved to {}", out.display());
    }
}
