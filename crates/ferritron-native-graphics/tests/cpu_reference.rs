//! CPU backend: reference pictures (M1, M4) and pixel checks.
mod common;
use common::*;
use fng::*;

fn render(scene: &Scene, scale: f32) -> RgbaImage {
    let mut cpu = CpuRenderer::new();
    let (px, w, h) = test_image();
    cpu.images().insert(&px, w, h);
    cpu.render(scene, ((W as f32 * scale) as u32, (H as f32 * scale) as u32), scale)
}

#[test]
fn reference_shapes() {
    check_reference("shapes", &render(&scene_shapes(), 1.0), 2, 0.01);
}

#[test]
fn reference_shapes_at_2x() {
    check_reference("shapes_2x", &render(&scene_shapes(), 2.0), 2, 0.01);
}

#[test]
fn reference_paths() {
    check_reference("paths", &render(&scene_paths(), 1.0), 2, 0.01);
}

#[test]
fn reference_paints() {
    check_reference("paints", &render(&scene_paints(), 1.0), 2, 0.01);
}

#[test]
fn reference_state() {
    check_reference("state", &render(&scene_state(), 1.0), 2, 0.01);
}

#[test]
fn a_layer_under_a_zoom_is_drawn_sharp() {
    // the layer's texture is drawn at the zoom of the transform it is placed with, so at 3x it
    // matches the same content drawn directly at 3x (a 1x texture would be blurred)
    let content = |s: &mut Scene| {
        s.fill_round_rect(Rect::new(1.0, 1.0, 8.0, 6.0), 2.0, Color::WHITE);
        s.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), Color::hex(0xff9f1c));
    };
    let mut direct = Scene::new();
    direct.push_transform(Affine::scale(3.0, 3.0));
    content(&mut direct);
    direct.pop_transform();
    let mut layered = Scene::new();
    layered.push_transform(Affine::scale(3.0, 3.0));
    layered.layer(LayerId(7), (10.0, 8.0), 1, |l| content(l));
    layered.pop_transform();
    let a = RenderCheck::render(&direct, (40, 30), 1.0);
    let b = RenderCheck::render(&layered, (40, 30), 1.0);
    let bad = a.data.iter().zip(&b.data).filter(|(p, q)| p.abs_diff(**q) > 2).count();
    assert!(bad < 10, "{bad} channel values differ by more than 2: the layer is blurred");
}

#[test]
fn a_shadow_moved_by_whole_pixels_is_the_same_picture() {
    // the coverage of a shadow depends on where its center falls between pixels, not on the
    // whole-pixel position: the second picture comes from the cached map of the first
    let shadow = |x: f32, y: f32| {
        let mut s = Scene::new();
        s.shadow(Rect::new(x, y, 50.0, 30.0), 6.0, ShadowStyle { blur: 14.0, offset: Point::new(1.0, 2.0), color: Color::BLACK });
        s
    };
    let a = RenderCheck::render(&shadow(20.25, 30.5), (120, 100), 1.0);
    let b = RenderCheck::render(&shadow(33.25, 37.5), (120, 100), 1.0);
    let mut moved = 0;
    for y in 0..100 - 7 {
        for x in 0..120 - 13 {
            if a.pixel(x, y) != b.pixel(x + 13, y + 7) {
                moved += 1;
            }
        }
    }
    assert_eq!(moved, 0, "{moved} pixels differ after the shift");
    assert!(a.pixel(45, 50)[3] > 200, "the shadow is drawn");
}

#[test]
fn rendering_is_deterministic() {
    let a = render(&scene_state(), 1.5);
    let b = render(&scene_state(), 1.5);
    assert_eq!(a.data, b.data);
}

#[test]
fn pixel_aligned_rect_is_exact() {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(2.0, 2.0, 4.0, 4.0), Color::rgb8(255, 0, 0));
    let img = RenderCheck::render(&s, (8, 8), 1.0);
    assert_eq!(img.pixel(2, 2), [255, 0, 0, 255]);
    assert_eq!(img.pixel(5, 5), [255, 0, 0, 255]);
    assert_eq!(img.pixel(1, 2), [0, 0, 0, 0]);
    assert_eq!(img.pixel(6, 5), [0, 0, 0, 0]);
}

struct RenderCheck;
impl RenderCheck {
    fn render(s: &Scene, size: (u32, u32), scale: f32) -> RgbaImage {
        CpuRenderer::new().render(s, size, scale)
    }
}

#[test]
fn half_covered_pixels_blend() {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::WHITE);
    s.fill_rect(Rect::new(1.5, 0.0, 1.0, 4.0), Color::BLACK);
    let img = RenderCheck::render(&s, (4, 4), 1.0);
    // half black over white = mid grey (coverage 0.5)
    let p = img.pixel(1, 1);
    assert!((p[0] as i32 - 128).abs() <= 1, "{p:?}");
    assert_eq!(img.pixel(2, 1)[0], p[0]);
    assert_eq!(img.pixel(0, 1), [255, 255, 255, 255]);
}

#[test]
fn scale_maps_logical_to_device_pixels() {
    let mut s = Scene::new();
    s.fill_rect(Rect::new(1.0, 1.0, 2.0, 2.0), Color::BLACK);
    let img = RenderCheck::render(&s, (8, 8), 2.0);
    assert_eq!(img.pixel(2, 2)[3], 255);
    assert_eq!(img.pixel(5, 5)[3], 255);
    assert_eq!(img.pixel(1, 1)[3], 0);
    assert_eq!(img.pixel(6, 6)[3], 0);
}

#[test]
fn opacity_group_is_composited_as_a_whole() {
    let mut s = Scene::new();
    s.push_opacity(0.5);
    s.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::rgb8(255, 0, 0));
    s.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::rgb8(0, 0, 255));
    s.pop_opacity();
    let img = RenderCheck::render(&s, (4, 4), 1.0);
    // blue over red inside the group is opaque blue; the group is then 50% transparent: no red
    let p = img.pixel(1, 1);
    assert_eq!(p[0], 0);
    assert!((p[3] as i32 - 128).abs() <= 1);
}

#[test]
fn clip_rect_limits_drawing() {
    let mut s = Scene::new();
    s.push_clip_rect(Rect::new(2.0, 2.0, 2.0, 2.0));
    s.fill_rect(Rect::new(0.0, 0.0, 8.0, 8.0), Color::BLACK);
    s.pop_clip();
    s.fill_rect(Rect::new(6.0, 6.0, 1.0, 1.0), Color::BLACK);
    let img = RenderCheck::render(&s, (8, 8), 1.0);
    assert_eq!(img.pixel(2, 2)[3], 255);
    assert_eq!(img.pixel(3, 3)[3], 255);
    assert_eq!(img.pixel(1, 1)[3], 0);
    assert_eq!(img.pixel(4, 4)[3], 0);
    assert_eq!(img.pixel(6, 6)[3], 255, "clip is popped");
}

#[test]
fn linear_gradient_endpoints_and_middle() {
    let mut s = Scene::new();
    s.fill_rect(
        Rect::new(0.0, 0.0, 100.0, 4.0),
        Paint::Linear { start: Point::new(0.0, 0.0), end: Point::new(100.0, 0.0), stops: Stops::two(Color::BLACK, Color::WHITE) },
    );
    let img = RenderCheck::render(&s, (100, 4), 1.0);
    assert!(img.pixel(0, 1)[0] <= 3);
    assert!((img.pixel(49, 1)[0] as i32 - 127).abs() <= 3);
    assert!(img.pixel(99, 1)[0] >= 252);
}

#[test]
fn image_is_drawn_with_nearest_sampling() {
    let mut cpu = CpuRenderer::new();
    let id = cpu.images().insert(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1);
    let mut s = Scene::new();
    s.image_sampled(id, Rect::new(0.0, 0.0, 8.0, 4.0), Sampling::Nearest);
    let img = cpu.render(&s, (8, 4), 1.0);
    assert_eq!(img.pixel(1, 1), [255, 0, 0, 255]);
    assert_eq!(img.pixel(6, 1), [0, 255, 0, 255]);
}

#[test]
fn layer_is_reused_when_the_version_is_unchanged() {
    let mut cpu = CpuRenderer::new();
    let mut s = Scene::new();
    let mut calls = 0;
    for (version, expect) in [(1u64, 255u8), (1, 255), (2, 0)] {
        s.clear();
        s.layer(LayerId(3), (4.0, 4.0), version, |l| {
            calls += 1;
            if version == 1 {
                l.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::BLACK);
            }
        });
        let img = cpu.render(&s, (4, 4), 1.0);
        assert_eq!(img.pixel(1, 1)[3], expect);
    }
    assert_eq!(calls, 2);
}

#[test]
fn stroke_widths_are_exact() {
    let mut s = Scene::new();
    s.stroke_line(Point::new(0.0, 4.0), Point::new(16.0, 4.0), &Stroke::width(4.0), Color::BLACK);
    let img = RenderCheck::render(&s, (16, 8), 1.0);
    assert_eq!(img.pixel(8, 1)[3], 0);
    assert_eq!(img.pixel(8, 2)[3], 255);
    assert_eq!(img.pixel(8, 5)[3], 255);
    assert_eq!(img.pixel(8, 6)[3], 0);
}

#[test]
fn dashes_leave_gaps() {
    let mut s = Scene::new();
    s.stroke_line(Point::new(0.0, 2.0), Point::new(16.0, 2.0), &Stroke::width(2.0).with_dash(Dash::new(4.0, 4.0)), Color::BLACK);
    let img = RenderCheck::render(&s, (16, 4), 1.0);
    assert_eq!(img.pixel(2, 2)[3], 255);
    assert_eq!(img.pixel(6, 2)[3], 0);
    assert_eq!(img.pixel(10, 2)[3], 255);
}

#[test]
fn an_inner_shadow_darkens_the_lit_side_inside_the_shape_only() {
    let mut s = Scene::new();
    s.fill_round_rect(Rect::new(20.0, 20.0, 80.0, 60.0), 10.0, Color::WHITE);
    s.inner_shadow(Rect::new(20.0, 20.0, 80.0, 60.0), 10.0, ShadowStyle { blur: 8.0, offset: Point::new(3.0, 3.0), color: Color::BLACK });
    let img = CpuRenderer::new().render(&s, (120, 100), 1.0);
    let lum = |x: u32, y: u32| img.pixel(x, y)[0] as i32;
    // along the top and left edges (offset down-right) it is dark, along the bottom and right light
    assert!(lum(60, 22) < 150, "top {}", lum(60, 22));
    assert!(lum(22, 50) < 150, "left {}", lum(22, 50));
    assert!(lum(60, 78) > 220, "bottom {}", lum(60, 78));
    assert!(lum(98, 50) > 220, "right {}", lum(98, 50));
    assert!(lum(60, 50) > 240, "middle {}", lum(60, 50));
    // nothing outside the shape
    assert_eq!(img.pixel(10, 10)[3], 0);
    assert_eq!(img.pixel(110, 90)[3], 0);
}

#[test]
fn a_picture_draws_like_the_same_commands_in_place() {
    let draw = |s: &mut Scene| {
        s.fill_round_rect(Rect::new(4.0, 4.0, 30.0, 20.0), 6.0, Color::hex(0x4f9dff));
        s.stroke_circle(Point::new(20.0, 14.0), 8.0, &Stroke::width(2.0), Color::WHITE);
    };
    let picture = Picture::record(draw);
    let mut direct = Scene::new();
    let mut placed = Scene::new();
    for (dx, dy, o) in [(0.0, 0.0, 1.0), (40.0, 10.0, 0.5)] {
        for (s, via) in [(&mut direct, false), (&mut placed, true)] {
            s.push_transform(Affine::translate(dx, dy));
            s.push_opacity(o);
            if via { s.draw_picture(&picture) } else { draw(s) }
            s.pop_opacity();
            s.pop_transform();
        }
    }
    let a = CpuRenderer::new().render(&direct, (90, 50), 1.0);
    let b = CpuRenderer::new().render(&placed, (90, 50), 1.0);
    assert_eq!(a.data, b.data);
}
